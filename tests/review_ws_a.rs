//! Regression tests for workstream WS-A of the October 2026 code and
//! security reviews: crashes (panics, stack overflow) and resource limits
//! (row cap, list cap, subquery limits).
//!
//! Each test fails on the pre-fix code; ids in comments match the review
//! reports (`sec #N` = security report, `code #N` = code report).

use graphdblite::{Config, Database, GraphError, QueryError, Record, Value};

fn mem() -> Database {
    Database::open_memory().unwrap()
}

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn err(db: &mut Database, q: &str) -> GraphError {
    match db.execute(q) {
        Ok(rows) => panic!("expected an error, got {} rows for {q}", rows.len()),
        Err(e) => e,
    }
}

/// Unwrap a `SizeLimit` error into `(what, limit, actual)`.
fn size_limit(e: GraphError) -> (String, usize, usize) {
    match e {
        GraphError::SizeLimit {
            what,
            limit,
            actual,
            ..
        } => (what, limit, actual),
        other => panic!("expected SizeLimit, got {other:?}"),
    }
}

fn open_with(config: Config) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with_config(dir.path().join("t.db"), config).unwrap();
    (dir, db)
}

// ── A1 (sec #1): temporal parsing must not panic on non-ASCII input ────────

/// Assert that `func(input)` fails with the temporal parse error rather than
/// panicking on a byte offset inside a multi-byte character.
fn assert_temporal_rejects(db: &mut Database, func: &str, what: &str, input: &str) {
    let q = format!("RETURN {func}('{input}') AS t");
    match db.execute(&q) {
        Err(GraphError::Query(e)) => {
            assert_eq!(
                e.code(),
                graphdblite::ErrorCode::InvalidArgumentValue,
                "{q}"
            );
            assert_eq!(
                e.message(),
                format!("temporal: invalid {what} string: contains non-ASCII characters"),
                "{q}"
            );
        }
        other => panic!("expected an InvalidArgumentValue error for {q}, got {other:?}"),
    }
}

#[test]
fn a1_date_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in [
        "202é101",
        "2020é01",
        "202é1",
        "2é20",
        "2020-0é-01",
        "2020-01-0é",
        "2020-W0é-1",
        "2020Wé1",
        "2020-01-01é",
        "é",
        "日本日本",
        "2020🎉",
    ] {
        assert_temporal_rejects(&mut db, "date", "date", input);
    }
}

#[test]
fn a1_time_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in [
        "1é1",
        "12é0",
        "1é1000",
        "12:00+1é1",
        "12:00+0é:00",
        "12:00+01:0é:00",
        "12:00:00.12345678é",
        "12:0é",
        "12:00:00Zé",
        "日本",
        "🎉",
    ] {
        assert_temporal_rejects(&mut db, "time", "time", input);
    }
}

#[test]
fn a1_localtime_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in ["1é1", "12é000", "12:00:00.12345678é", "1é", "日本"] {
        assert_temporal_rejects(&mut db, "localtime", "local time", input);
    }
}

#[test]
fn a1_datetime_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in [
        "2020-01-01T00:00+1é1",
        "202é101T12:00Z",
        "2020-01-01T1é1Z",
        "2020-01-01T12:00:00.12345678éZ",
        "2020-01-01T12:00[Europé/Berlin]",
        "2020-W0é-1T12:00Z",
        "日本T12:00Z",
    ] {
        assert_temporal_rejects(&mut db, "datetime", "datetime", input);
    }
}

#[test]
fn a1_localdatetime_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in [
        "2020-01-01T1é1",
        "202é101T12:00",
        "2020-01-01T12:00:00.12345678é",
    ] {
        assert_temporal_rejects(&mut db, "localdatetime", "local datetime", input);
    }
}

#[test]
fn a1_duration_non_ascii_is_a_parse_error() {
    let mut db = mem();
    for input in [
        "P1é",
        "PT1é",
        "P1Y2é3D",
        "PT1.12345678éS",
        "Pé",
        "P1éY",
        "PT1H2é3M",
    ] {
        assert_temporal_rejects(&mut db, "duration", "duration", input);
    }
}

#[test]
fn a1_stored_non_ascii_string_read_back_through_time_is_an_error() {
    // The persistent variant: a bad string stored as a property and later
    // parsed with `time(n.t)` must error the same way, every time.
    let mut db = mem();
    run(&mut db, "CREATE (:T {t: '1é1'})");
    for _ in 0..2 {
        match db.execute("MATCH (n:T) RETURN time(n.t) AS t") {
            Err(GraphError::Query(e)) => {
                assert_eq!(e.code(), graphdblite::ErrorCode::InvalidArgumentValue);
                assert_eq!(
                    e.message(),
                    "temporal: invalid time string: contains non-ASCII characters"
                )
            }
            other => panic!("expected an InvalidArgumentValue error, got {other:?}"),
        }
    }
    // Valid ASCII input still parses (good partition).
    let rows = run(&mut db, "RETURN toString(time('12:00+01:00')) AS t");
    assert_eq!(rows[0].get("t"), Some(&Value::String("12:00+01:00".into())));
}

// ── A3 (sec #2, code #19): flat operator chains must not overflow the stack ─

const MAX_EXPR_DEPTH: usize = 256;

/// `RETURN 1 + 1 + ... + 1` with `terms` operands. The expression tree has
/// depth `terms` (a left-deep chain of `terms - 1` additions over leaves).
fn add_chain(terms: usize) -> String {
    let mut q = String::from("RETURN 1");
    for _ in 1..terms {
        q.push_str(" + 1");
    }
    q.push_str(" AS v");
    q
}

/// Run `q` on a thread with a 64 MiB stack and return its single value.
///
/// Unoptimized builds use roughly 23 KiB of stack per expression level
/// (release builds about 1 KiB), so evaluating a depth-256 expression needs
/// more than the 2 MiB default test-thread stack in a debug test run.
fn eval_on_big_stack(q: String) -> Value {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut db = mem();
            let rows = run(&mut db, &q);
            rows[0].get("v").cloned().unwrap()
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn a3_add_chain_at_depth_limit_is_accepted() {
    let v = eval_on_big_stack(add_chain(MAX_EXPR_DEPTH));
    assert_eq!(v, Value::I64(MAX_EXPR_DEPTH as i64));
}

#[test]
fn a3_add_chain_over_depth_limit_is_size_limit() {
    let mut db = mem();
    let (what, limit, actual) = size_limit(err(&mut db, &add_chain(MAX_EXPR_DEPTH + 1)));
    assert_eq!(what, "expression nesting depth");
    assert_eq!(limit, MAX_EXPR_DEPTH);
    assert_eq!(actual, MAX_EXPR_DEPTH + 1);
}

#[test]
fn a3_huge_flat_chains_are_rejected_without_crashing() {
    let mut db = mem();
    // Pre-fix, debug builds overflowed below 100 levels and release builds
    // below 2,000 on a 2 MiB stack. The add chain uses the review's 100k
    // repro; the rest use 20k to keep the (debug-build) pest parse fast.
    let n = 20_000;
    let queries = [
        add_chain(100_000),
        format!("RETURN {}true AS v", "NOT ".repeat(n)),
        format!("RETURN true{} AS v", " AND true".repeat(n)),
        format!("RETURN true{} AS v", " OR true".repeat(n)),
        format!("RETURN 2{} AS v", " * 1".repeat(n)),
        format!("RETURN 1{} AS v", " < 2".repeat(n)),
        format!("RETURN {}1 AS v", "- ".repeat(n)),
        format!("WITH {{a: 1}} AS m RETURN m{} AS v", ".a".repeat(n)),
        format!("WITH [1] AS l RETURN l{} AS v", "[0]".repeat(n)),
        // CASE nests without brackets; 30k levels stay under the 1 MiB
        // query cap so the depth check (not the size check) must fire.
        format!(
            "RETURN {}1{} AS v",
            "CASE WHEN true THEN ".repeat(30_000),
            " END".repeat(30_000)
        ),
    ];
    for q in &queries {
        let (what, limit, _) = size_limit(err(&mut db, q));
        assert_eq!(what, "expression nesting depth", "{}", &q[..40]);
        assert_eq!(limit, MAX_EXPR_DEPTH);
    }
}

#[test]
fn a3_not_chain_boundary() {
    // `NOT` x 255 over a literal is depth 256: accepted (odd count -> false).
    let ok = format!("RETURN {}true AS v", "NOT ".repeat(MAX_EXPR_DEPTH - 1));
    assert_eq!(eval_on_big_stack(ok), Value::Bool(false));
    let mut db = mem();
    let bad = format!("RETURN {}true AS v", "NOT ".repeat(MAX_EXPR_DEPTH));
    let (what, _, actual) = size_limit(err(&mut db, &bad));
    assert_eq!(what, "expression nesting depth");
    assert_eq!(actual, MAX_EXPR_DEPTH + 1);
}

#[test]
fn a3_depth_composes_across_parentheses() {
    // Two chains of 200 nested via parentheses are 400 deep in total even
    // though neither the bracket depth nor either chain exceeds the cap.
    let mut db = mem();
    let inner = add_chain(200)
        .trim_start_matches("RETURN ")
        .trim_end_matches(" AS v")
        .to_string();
    let q = format!("RETURN ({inner}){} AS v", " + 1".repeat(200));
    let (what, _, actual) = size_limit(err(&mut db, &q));
    assert_eq!(what, "expression nesting depth");
    assert_eq!(actual, MAX_EXPR_DEPTH + 1);
}

#[test]
fn a3_pattern_with_thousands_of_inline_properties_runs() {
    // Inline pattern properties become one AND predicate; built left-deep,
    // 5,000 of them overflowed the stack (even in release on a 2 MiB stack).
    let mut db = mem();
    let n = 5_000;
    let props: std::collections::BTreeMap<String, Value> =
        (0..n).map(|i| (format!("p{i}"), Value::I64(i))).collect();
    let params = std::collections::HashMap::from([("m".to_string(), Value::Map(props))]);
    db.execute_with_params("CREATE (w:W) SET w = $m", Some(&params))
        .unwrap();
    let pattern = |last: i64| {
        let mut parts: Vec<String> = (0..n - 1).map(|i| format!("p{i}: {i}")).collect();
        parts.push(format!("p{}: {last}", n - 1));
        format!("MATCH (w:W {{{}}}) RETURN count(w) AS c", parts.join(", "))
    };
    let count = |db: &mut Database, q: &str| run(db, q)[0].get("c").cloned();
    assert_eq!(count(&mut db, &pattern(n - 1)), Some(Value::I64(1)));
    // One mismatching property (the last conjunct) must filter the node out.
    assert_eq!(count(&mut db, &pattern(-1)), Some(Value::I64(0)));
}

/// A WHERE predicate whose leftmost (deepest) operand is a nested EXISTS:
/// `CASE WHEN EXISTS { MATCH (x:N) WHERE <inner> } THEN 1 ELSE 0 END + 1 + ...`.
/// Each level is ~200 deep, under the parser cap, but evaluation recurses
/// through every level on one thread.
fn nested_exists_predicate(levels: usize) -> String {
    let mut pred = "true".to_string();
    for _ in 0..levels {
        pred = format!(
            "CASE WHEN EXISTS {{ MATCH (x:N) WHERE {pred} }} THEN 1 ELSE 0 END{} > 0",
            " + 1".repeat(200)
        );
    }
    format!("MATCH (n:N) WHERE {pred} RETURN count(n) AS c")
}

#[test]
fn a3_eval_depth_across_nested_subqueries_is_size_limit() {
    let outcome = |levels: usize| {
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn(move || {
                let mut db = mem();
                run(&mut db, "CREATE (:N)");
                db.execute(&nested_exists_predicate(levels))
            })
            .unwrap()
            .join()
            .unwrap()
    };
    // Two levels (~400 eval frames) fit under the 512 evaluation cap.
    let rows = outcome(2).unwrap();
    assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
    // Three levels (~600) exceed it.
    let (what, limit, _) = size_limit(outcome(3).unwrap_err());
    assert_eq!(what, "expression evaluation depth");
    assert_eq!(limit, 2 * MAX_EXPR_DEPTH);
}

// ── A4 (sec #4, code #25): subqueries run under the caller's limits ────────

/// A chain `(:C:S {i: 0})-[:NEXT]->(:C {i: 1})...->(:C {i: len-1})` in a
/// database opened with `config`. Built by one path CREATE so setup never
/// trips a low row cap under test; `:S` marks the start node so a query can
/// scan it without scanning (and row-counting) the whole chain.
fn chain_db(len: usize, config: Config) -> (tempfile::TempDir, Database) {
    let (dir, mut db) = open_with(config);
    let nodes: Vec<String> = (0..len)
        .map(|i| match i {
            0 => "(:C:S {i: 0})".to_string(),
            _ => format!("(:C {{i: {i}}})"),
        })
        .collect();
    run(&mut db, &format!("CREATE {}", nodes.join("-[:NEXT]->")));
    (dir, db)
}

/// The top-level traversal-depth error for a `hops` hop pattern.
fn depth_error_text(hops: u32, cap: u32) -> String {
    format!("var-length hop count `{hops}` exceeds the configured max traversal depth of `{cap}`")
}

fn assert_depth_error(e: GraphError, hops: u32, cap: u32) {
    match &e {
        GraphError::Query(QueryError::ArithmeticError { message, .. }) => {
            assert_eq!(message, &depth_error_text(hops, cap))
        }
        other => panic!("expected the traversal depth error, got {other:?}"),
    }
}

/// Subquery forms that each contain a `[*1..{hops}]` traversal from `a`.
fn subquery_forms(hops: u32) -> [String; 4] {
    [
        format!(
            "MATCH (a:C {{i: 0}}) WHERE EXISTS {{ MATCH (a)-[*1..{hops}]->(b) }} RETURN a.i AS v"
        ),
        format!("MATCH (a:C {{i: 0}}) WHERE (a)-[*1..{hops}]->() RETURN a.i AS v"),
        format!("MATCH (a:C {{i: 0}}) RETURN size([(a)-[*1..{hops}]->(b) | b]) AS v"),
        format!(
            "MATCH (a:C {{i: 0}}) WHERE EXISTS {{ MATCH (a)-[*1..{hops}]->(b) RETURN b }} \
             RETURN a.i AS v"
        ),
    ]
}

#[test]
fn a4_top_level_depth_error_is_the_reference() {
    // Sanity anchor: the top-level query already fails this way.
    let (_d, mut db) = chain_db(3, Config::default());
    let e = err(&mut db, "MATCH (a:C {i: 0})-[*1..100]->(b) RETURN b");
    assert_depth_error(e, 100, 64);
}

#[test]
fn a4_subqueries_respect_max_traversal_depth() {
    let (_d, mut db) = chain_db(3, Config::default());
    for q in subquery_forms(100) {
        assert_depth_error(err(&mut db, &q), 100, 64);
    }
    // Boundary: 65 hops is rejected, 64 (the cap) is accepted.
    for q in subquery_forms(65) {
        assert_depth_error(err(&mut db, &q), 65, 64);
    }
    let expected = [Value::I64(0), Value::I64(0), Value::I64(2), Value::I64(0)];
    for (q, want) in subquery_forms(64).iter().zip(expected) {
        assert_eq!(run(&mut db, q)[0].get("v"), Some(&want), "{q}");
    }
}

#[test]
fn a4_subquery_depth_cap_follows_config() {
    // A lowered cap applies inside subqueries; 0 disables it.
    let (_d, mut low) = chain_db(
        3,
        Config {
            max_traversal_depth: 2,
            ..Default::default()
        },
    );
    for q in subquery_forms(3) {
        assert_depth_error(err(&mut low, &q), 3, 2);
    }
    let (_d, mut off) = chain_db(
        3,
        Config {
            max_traversal_depth: 0,
            ..Default::default()
        },
    );
    let rows = run(&mut off, &subquery_forms(100)[2]);
    assert_eq!(rows[0].get("v"), Some(&Value::I64(2)));
}

#[test]
fn a4_configured_traversal_work_applies_inside_exists() {
    let q = "MATCH (a:C {i: 0}) WHERE EXISTS { MATCH (a)-[*1..9]->(b:C {i: 9}) } RETURN a.i AS v";
    // Reaching i=9 needs 9 edge visits: a budget of 5 must fail.
    let (_d, mut small) = chain_db(
        10,
        Config {
            max_traversal_work: 5,
            ..Default::default()
        },
    );
    let (what, limit, _) = size_limit(err(&mut small, q));
    assert_eq!(what, "variable-length traversal work");
    assert_eq!(limit, 5);
    // The same budget applies to a pattern comprehension.
    let (what, _, _) = size_limit(err(
        &mut small,
        "MATCH (a:C {i: 0}) RETURN size([(a)-[*1..9]->(b) | b]) AS v",
    ));
    assert_eq!(what, "variable-length traversal work");
    // 0 disables the budget.
    let (_d, mut unlimited) = chain_db(
        10,
        Config {
            max_traversal_work: 0,
            ..Default::default()
        },
    );
    assert_eq!(run(&mut unlimited, q)[0].get("v"), Some(&Value::I64(0)));
}

#[test]
fn a4_row_cap_applies_to_pattern_comprehension() {
    // The comprehension yields 9 rows (one per reachable node); the outer
    // query only ever holds one row.
    let q = "MATCH (a:S) RETURN size([(a)-[*1..9]->(b) | b]) AS v";
    let cap = |rows: usize| {
        chain_db(
            10,
            Config {
                max_result_rows: rows,
                ..Default::default()
            },
        )
    };
    let (_d, mut low) = cap(8);
    match err(&mut low, q) {
        GraphError::Query(QueryError::ConstraintViolation { message, .. }) => {
            assert_eq!(message, "result set exceeded maximum of 8 rows")
        }
        other => panic!("expected the row cap error, got {other:?}"),
    }
    let (_d, mut exact) = cap(9);
    assert_eq!(run(&mut exact, q)[0].get("v"), Some(&Value::I64(9)));
    let (_d, mut off) = cap(0);
    assert_eq!(run(&mut off, q)[0].get("v"), Some(&Value::I64(9)));
}

#[test]
fn a4_shortest_path_budget_is_shared_with_subqueries() {
    // Each search from i=0 to i=9 costs 9 edge visits. The outer search
    // and the one inside EXISTS draw on one per-query budget.
    let q = "MATCH (a:S), (b:C {i: 9}), p = shortestPath((a)-[*]->(b)) \
             WHERE EXISTS { MATCH (x:S), (y:C {i: 9}), q = shortestPath((x)-[*]->(y)) RETURN q } \
             RETURN length(p) AS v";
    let budget = |work: u64| {
        chain_db(
            10,
            Config {
                max_traversal_work: work,
                ..Default::default()
            },
        )
    };
    // 18 visits in total: a budget of 17 fails although each search fits.
    let (_d, mut tight) = budget(17);
    let (what, limit, _) = size_limit(err(&mut tight, q));
    assert_eq!(what, "shortestPath traversal work");
    assert_eq!(limit, 17);
    let (_d, mut exact) = budget(18);
    assert_eq!(run(&mut exact, q)[0].get("v"), Some(&Value::I64(9)));
}

// ── A5 / A6 (sec #3, #6): the row cap holds on every execution path ────────

fn assert_row_cap_error(e: GraphError, cap: usize) {
    match e {
        GraphError::Query(QueryError::ConstraintViolation { message, .. }) => {
            assert_eq!(
                message,
                format!("result set exceeded maximum of {cap} rows")
            )
        }
        other => panic!("expected the row cap error, got {other:?}"),
    }
}

fn capped(max_result_rows: usize) -> (tempfile::TempDir, Database) {
    open_with(Config {
        max_result_rows,
        ..Default::default()
    })
}

#[test]
fn a5_slot_path_enforces_default_row_cap() {
    // The review repro: 150,000 rows came back despite the 100K default.
    let mut db = mem();
    assert_row_cap_error(
        err(&mut db, "UNWIND range(1, 150000) AS x RETURN x"),
        100_000,
    );
    // Boundary at the default cap: exactly 100,000 rows is fine, one more is not.
    assert_eq!(
        run(&mut db, "UNWIND range(1, 100000) AS x RETURN x").len(),
        100_000
    );
    assert_row_cap_error(
        err(&mut db, "UNWIND range(1, 100001) AS x RETURN x"),
        100_000,
    );
}

#[test]
fn a5_slot_path_row_cap_boundaries() {
    let (_d, mut db) = capped(10);
    let rows = run(&mut db, "UNWIND range(1, 10) AS x RETURN x");
    let got: Vec<&Value> = rows.iter().map(|r| r.get("x").unwrap()).collect();
    let want: Vec<Value> = (1..=10).map(Value::I64).collect();
    assert_eq!(got, want.iter().collect::<Vec<_>>());
    assert_row_cap_error(err(&mut db, "UNWIND range(1, 11) AS x RETURN x"), 10);
    // A cap of 1: one row passes, two fail.
    let (_d, mut one) = capped(1);
    assert_eq!(run(&mut one, "RETURN 1 AS x").len(), 1);
    assert_row_cap_error(err(&mut one, "UNWIND [1, 2] AS x RETURN x"), 1);
    // 0 disables the cap.
    let (_d, mut off) = capped(0);
    assert_eq!(
        run(&mut off, "UNWIND range(1, 150000) AS x RETURN x").len(),
        150_000
    );
}

#[test]
fn a6_cartesian_stops_at_the_cap_instead_of_materializing() {
    // 40^4 = 2.56M rows if fully built. Both the comma form (CrossProduct)
    // and the separate-MATCH form (CorrelatedJoin) must stop at row cap+1.
    let (_d, mut db) = capped(1_000);
    run(&mut db, "UNWIND range(1, 40) AS i CREATE (:N {i: i})");
    for q in [
        "MATCH (a:N), (b:N), (c:N), (d:N) RETURN count(*) AS c",
        "MATCH (a:N) MATCH (b:N) MATCH (c:N) MATCH (d:N) RETURN count(*) AS c",
    ] {
        let start = std::time::Instant::now();
        assert_row_cap_error(err(&mut db, q), 1_000);
        let took = start.elapsed();
        assert!(took.as_secs() < 5, "{q} took {took:?}");
    }
}

#[test]
fn a6_correlated_join_row_cap_boundary() {
    // Two separate MATCHes over 10 nodes join into exactly 100 rows.
    let q = "MATCH (a:N) MATCH (b:N) RETURN count(*) AS c";
    let (_d, mut exact) = capped(100);
    run(&mut exact, "UNWIND range(1, 10) AS i CREATE (:N {i: i})");
    assert_eq!(run(&mut exact, q)[0].get("c"), Some(&Value::I64(100)));
    let (_d, mut over) = capped(99);
    run(&mut over, "UNWIND range(1, 10) AS i CREATE (:N {i: i})");
    assert_row_cap_error(err(&mut over, q), 99);
}

#[test]
fn a6_optional_match_row_cap_boundary() {
    // OPTIONAL MATCH (LeftOuterJoin) over 10 x 10 nodes: 100 rows.
    let q = "MATCH (a:N) OPTIONAL MATCH (b:N) RETURN count(*) AS c";
    let (_d, mut exact) = capped(100);
    run(&mut exact, "UNWIND range(1, 10) AS i CREATE (:N {i: i})");
    assert_eq!(run(&mut exact, q)[0].get("c"), Some(&Value::I64(100)));
    let (_d, mut over) = capped(99);
    run(&mut over, "UNWIND range(1, 10) AS i CREATE (:N {i: i})");
    assert_row_cap_error(err(&mut over, q), 99);
}

#[test]
fn a6_sort_input_is_capped() {
    // ORDER BY buffers its whole input; the final LIMIT does not shrink it.
    let (_d, mut db) = capped(1_000);
    let ok = run(
        &mut db,
        "UNWIND range(1, 1000) AS x RETURN x ORDER BY x DESC LIMIT 1",
    );
    assert_eq!(ok[0].get("x"), Some(&Value::I64(1000)));
    assert_row_cap_error(
        err(
            &mut db,
            "UNWIND range(1, 1001) AS x RETURN x ORDER BY x DESC LIMIT 1",
        ),
        1_000,
    );
}

#[test]
fn a6_unwind_is_not_row_capped_when_its_rows_are_consumed() {
    // UNWIND output is bounded by the list cap, not the row cap: bulk loads
    // and aggregates over a long UNWIND keep working under a low row cap
    // because their results stay small.
    let (_d, mut db) = capped(100);
    run(&mut db, "UNWIND range(1, 1000) AS i CREATE (:U {i: i})");
    assert_eq!(
        run(&mut db, "UNWIND range(1, 1000) AS i RETURN sum(i) AS s")[0].get("s"),
        Some(&Value::I64(500_500))
    );
    assert_eq!(
        run(
            &mut db,
            "UNWIND range(1, 1000) AS i RETURN size(collect(i)) AS s"
        )[0]
        .get("s"),
        Some(&Value::I64(1000))
    );
}

#[test]
fn a6_bare_write_output_is_not_row_capped() {
    // A write with no RETURN returns no rows, so the cap does not apply to
    // the rows it processes; with RETURN, the result rows are capped.
    let (_d, mut db) = capped(100);
    assert!(run(&mut db, "UNWIND range(1, 1000) AS i CREATE (:B {i: i})").is_empty());
    assert_row_cap_error(
        err(
            &mut db,
            "UNWIND range(1, 101) AS i CREATE (b:R {i: i}) RETURN b.i AS i",
        ),
        100,
    );
}

// ── A7 (sec #5): one list-size cap for every list built by a query ─────────

/// The cap is 10M values (`range()`'s limit), counting nested contents.
const MAX_LIST_LEN: usize = 10_000_000;

fn assert_list_cap_error(e: GraphError, actual: usize) {
    let (what, limit, got) = size_limit(e);
    assert_eq!(what, "list size");
    assert_eq!(limit, MAX_LIST_LEN);
    assert_eq!(got, actual);
}

#[test]
fn a7_small_lists_are_unaffected() {
    // Good partition: every list-building path still returns exact values.
    let mut db = mem();
    let q = "WITH [1, [2, 3]] + [4] AS a, [x IN range(1, 3) | [x, x]] AS b \
             UNWIND [1, 2] AS u \
             RETURN a, b, collect(u) AS c";
    let rows = run(&mut db, q);
    let l = |v: Vec<Value>| Value::List(v);
    let i = Value::I64;
    assert_eq!(
        rows[0].get("a"),
        Some(&l(vec![i(1), l(vec![i(2), i(3)]), i(4)]))
    );
    assert_eq!(
        rows[0].get("b"),
        Some(&l(vec![
            l(vec![i(1), i(1)]),
            l(vec![i(2), i(2)]),
            l(vec![i(3), i(3)])
        ]))
    );
    assert_eq!(rows[0].get("c"), Some(&l(vec![i(1), i(2)])));
}

#[test]
#[ignore = "allocates ~1 GB; run with `cargo test --release -- --ignored`"]
fn a7_composed_lists_hit_the_cap() {
    let mut db = mem();
    // Concatenation: 6M + (4M + 1) = 10M + 1 values, rejected before the
    // combined list is allocated. The review repro OOM-aborted.
    assert_list_cap_error(
        err(
            &mut db,
            "RETURN size(range(1, 6000000) + range(0, 4000000)) AS n",
        ),
        MAX_LIST_LEN + 1,
    );
    // Exactly at the cap is fine.
    let rows = run(
        &mut db,
        "RETURN size(range(1, 6000000) + range(1, 4000000)) AS n",
    );
    assert_eq!(rows[0].get("n"), Some(&Value::I64(MAX_LIST_LEN as i64)));
    // Comprehension output counts nested values: 4M elements of [x, x]
    // pass the cap at element 3,333,334 (3 values each).
    assert_list_cap_error(
        err(
            &mut db,
            "RETURN size([x IN range(1, 4000000) | [x, x]]) AS n",
        ),
        3_333_334 * 3,
    );
    // A list literal holding big lists counts their contents.
    assert_list_cap_error(
        err(
            &mut db,
            "WITH range(1, 6000000) AS r RETURN size([r, range(1, 4000000)]) AS n",
        ),
        2 + MAX_LIST_LEN,
    );
}
