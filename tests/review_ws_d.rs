//! Regression tests for the planner/parser findings of the October 2026 code
//! review (workstream D). Test names carry the finding id (D1..D9) and each
//! one fails on the pre-fix code.

use std::collections::HashMap;

use graphdblite::{Database, ErrorCode, GraphError, Record, Value};

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn run_p(db: &mut Database, q: &str, params: &[(&str, Value)]) -> Vec<Record> {
    let p: HashMap<String, Value> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    db.execute_with_params(q, Some(&p))
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

/// Single-row, single-column integer result.
fn int(db: &mut Database, q: &str) -> i64 {
    let rows = run(db, q);
    assert_eq!(rows.len(), 1, "expected one row from {q}, got {rows:?}");
    let first = rows[0].values().next().cloned();
    match first {
        Some(Value::I64(n)) => n,
        other => panic!("expected integer from {q}, got {other:?}"),
    }
}

/// Column `col` of every row, in row order.
fn col(rows: &[Record], col: &str) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            r.get(col)
                .cloned()
                .unwrap_or_else(|| panic!("missing column {col} in {r:?}"))
        })
        .collect()
}

fn ints(rows: &[Record], c: &str) -> Vec<i64> {
    col(rows, c)
        .into_iter()
        .map(|v| match v {
            Value::I64(n) => n,
            other => panic!("expected integer in {c}, got {other:?}"),
        })
        .collect()
}

fn sorted_strings(rows: &[Record], c: &str) -> Vec<String> {
    let mut out: Vec<String> = col(rows, c)
        .into_iter()
        .map(|v| match v {
            Value::String(s) => s,
            other => panic!("expected string in {c}, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn explain(db: &mut Database, q: &str) -> String {
    let rows = run(db, &format!("EXPLAIN {q}"));
    match rows[0].get("plan") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("expected plan text, got {other:?}"),
    }
}

/// Assert `q` fails with a query error carrying `code`.
fn expect_code(db: &mut Database, q: &str, code: ErrorCode) {
    match db.execute(q) {
        Err(GraphError::Query(qe)) => assert_eq!(qe.code(), code, "wrong code for {q}: {qe}"),
        other => panic!("expected {code:?} error from {q}, got {other:?}"),
    }
}

// ── D1: composite index only when every column is constrained ──────────────

fn composite_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:P {x: 1, y: 'd'}), (:P {x: 1, z: 5, y: 'f'}), (:P {x: 2, z: 5, y: 'g'})",
    );
    db.begin_write().unwrap();
    db.create_composite_index("P", &["x", "z"]).unwrap();
    db.commit().unwrap();
    db
}

#[test]
fn d1_composite_prefix_keeps_nodes_missing_trailing_column() {
    let mut db = composite_db();
    let rows = run(&mut db, "MATCH (n:P) WHERE n.x = 1 RETURN n.y");
    assert_eq!(sorted_strings(&rows, "n.y"), vec!["d", "f"]);
    let rows = run(&mut db, "MATCH (n:P {x: 1}) RETURN n.y");
    assert_eq!(sorted_strings(&rows, "n.y"), vec!["d", "f"]);
}

#[test]
fn d1_composite_prefix_only_does_not_pick_composite_index() {
    let mut db = composite_db();
    let plan = explain(&mut db, "MATCH (n:P) WHERE n.x = 1 RETURN n.y");
    assert!(
        !plan.contains("IndexLookup"),
        "prefix-only must scan:\n{plan}"
    );
}

#[test]
fn d1_composite_full_match_still_uses_index() {
    let mut db = composite_db();
    let q = "MATCH (n:P) WHERE n.x = 1 AND n.z = 5 RETURN n.y";
    let plan = explain(&mut db, q);
    assert!(
        plan.contains("IndexLookup :P.(x = I64(1), z = I64(5))"),
        "full match must use the composite index:\n{plan}"
    );
    assert_eq!(sorted_strings(&run(&mut db, q), "n.y"), vec!["f"]);
}

#[test]
fn d1_single_prop_index_preferred_over_composite_prefix() {
    let mut db = composite_db();
    db.begin_write().unwrap();
    db.create_index("P", "x").unwrap();
    db.commit().unwrap();
    let q = "MATCH (n:P) WHERE n.x = 1 RETURN n.y";
    let plan = explain(&mut db, q);
    assert!(
        plan.contains("IndexLookup :P.(x = I64(1))"),
        "single-prop index should serve x = 1:\n{plan}"
    );
    assert_eq!(sorted_strings(&run(&mut db, q), "n.y"), vec!["d", "f"]);
}

// ── D2: WITH ... ORDER BY ... SKIP/LIMIT ... WHERE ordering ────────────────

fn v_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE ({v: 3, w: 1}), ({v: 1, w: 2}), ({v: 2, w: 3})",
    );
    db
}

#[test]
fn d2_with_where_applies_after_limit() {
    let mut db = v_db();
    let rows = run(
        &mut db,
        "MATCH (n) WITH n ORDER BY n.v DESC LIMIT 1 WHERE n.v < 3 RETURN n.v",
    );
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
}

#[test]
fn d2_with_where_keeps_sort_order() {
    let mut db = v_db();
    let rows = run(
        &mut db,
        "MATCH (n) WITH n ORDER BY n.v DESC WHERE n.v < 3 RETURN collect(n.v) AS c",
    );
    assert_eq!(
        col(&rows, "c"),
        vec![Value::List(vec![Value::I64(2), Value::I64(1)])]
    );
}

#[test]
fn d2_with_where_applies_after_skip() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3] AS x WITH x ORDER BY x DESC SKIP 1 WHERE x > 1 RETURN x",
    );
    assert_eq!(ints(&rows, "x"), vec![2]);
}

#[test]
fn d2_aggregate_with_where_applies_after_limit() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3, 4, 5] AS x WITH x % 2 AS k, count(*) AS c ORDER BY k LIMIT 1 \
         WHERE k = 1 RETURN k, c",
    );
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
    // Without LIMIT the filter keeps the k = 1 group.
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3, 4, 5] AS x WITH x % 2 AS k, count(*) AS c ORDER BY k \
         WHERE k = 1 RETURN k, c",
    );
    assert_eq!(ints(&rows, "k"), vec![1]);
    assert_eq!(ints(&rows, "c"), vec![3]);
}

#[test]
fn d2_with_where_on_pre_projection_variable_respects_limit() {
    // WHERE may still reference a variable WITH did not project (non-DISTINCT,
    // non-aggregating WITH); it must apply after ORDER BY + LIMIT.
    let mut db = v_db();
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v AS v ORDER BY n.w LIMIT 2 WHERE n.w > 1 RETURN v",
    );
    assert_eq!(ints(&rows, "v"), vec![1]);
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v AS v WHERE n.w > 1 RETURN v ORDER BY v",
    );
    assert_eq!(ints(&rows, "v"), vec![1, 2]);
}

#[test]
fn d2_with_where_alias_after_order_by_and_limit() {
    let mut db = v_db();
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v AS v ORDER BY v LIMIT 2 WHERE v > 1 RETURN v",
    );
    assert_eq!(ints(&rows, "v"), vec![2]);
}

#[test]
fn d2_with_distinct_where_after_limit() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND [3, 3, 1, 2, 2] AS x WITH DISTINCT x ORDER BY x DESC LIMIT 2 WHERE x < 3 RETURN x",
    );
    assert_eq!(ints(&rows, "x"), vec![2]);
}

#[test]
fn d2_with_order_by_undefined_variable_errors_with_where() {
    let mut db = v_db();
    expect_code(
        &mut db,
        "MATCH (n) WITH n.v AS v ORDER BY zzz RETURN v",
        ErrorCode::UndefinedVariable,
    );
    expect_code(
        &mut db,
        "MATCH (n) WITH n.v AS v ORDER BY zzz WHERE v > 0 RETURN v",
        ErrorCode::UndefinedVariable,
    );
}

#[test]
fn d2_with_distinct_where_on_dropped_variable_errors() {
    let mut db = v_db();
    expect_code(
        &mut db,
        "MATCH (n) WITH DISTINCT n.v AS v WHERE n.w > 1 RETURN v",
        ErrorCode::UndefinedVariable,
    );
    // Repeating a projected expression verbatim reads its projected value.
    let rows = run(
        &mut db,
        "MATCH (n) WITH DISTINCT n.v AS v ORDER BY v LIMIT 2 WHERE n.v > 1 RETURN v",
    );
    assert_eq!(ints(&rows, "v"), vec![2]);
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v % 2 AS k, count(*) AS c WHERE n.v % 2 = 1 RETURN k, c",
    );
    assert_eq!(ints(&rows, "k"), vec![1]);
    assert_eq!(ints(&rows, "c"), vec![2]);
}

#[test]
fn d2_aggregate_with_order_by_group_key_alias() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3, 4, 5] AS x WITH x % 2 AS k, count(*) AS c ORDER BY k RETURN k, c",
    );
    assert_eq!(ints(&rows, "k"), vec![0, 1]);
    assert_eq!(ints(&rows, "c"), vec![2, 3]);
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3] AS x WITH x AS k, count(*) AS c ORDER BY k DESC RETURN k",
    );
    assert_eq!(ints(&rows, "k"), vec![3, 2, 1]);
}

#[test]
fn d2_with_where_undefined_variable_errors() {
    let mut db = v_db();
    expect_code(
        &mut db,
        "MATCH (n) WITH n WHERE m.v > 1 RETURN n.v",
        ErrorCode::UndefinedVariable,
    );
    // An aggregating WITH drops every non-projected variable.
    expect_code(
        &mut db,
        "MATCH (n) WITH n.v AS v, count(*) AS c WHERE n.w > 1 RETURN v",
        ErrorCode::UndefinedVariable,
    );
}

#[test]
fn d2_with_where_projected_forms() {
    let mut db = v_db();
    // Unaliased property projection.
    assert_eq!(
        int(&mut db, "MATCH (n) WITH n.v WHERE n.v > 1 RETURN count(*)"),
        2
    );
    // WITH * keeps everything.
    let rows = run(&mut db, "MATCH (n) WITH * WHERE n.v > 2 RETURN n.w");
    assert_eq!(ints(&rows, "n.w"), vec![1]);
    // Quantifier locals are not dropped variables.
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v AS v WHERE any(x IN [1, 2] WHERE x = v) RETURN v ORDER BY v",
    );
    assert_eq!(ints(&rows, "v"), vec![1, 2]);
}

#[test]
fn d2_with_where_pattern_predicate() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:A {name: 'a1'})-[:R]->(:B), (:A {name: 'a2'})",
    );
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a ORDER BY a.name DESC LIMIT 1 WHERE (a)-[:R]->() RETURN a.name",
    );
    assert!(rows.is_empty(), "a2 (first after DESC) has no R: {rows:?}");
    let rows = run(
        &mut db,
        "MATCH (a:A) WITH a.name AS name WHERE (a)-[:R]->() RETURN name",
    );
    assert_eq!(sorted_strings(&rows, "name"), vec!["a1"]);
    let rows = run(
        &mut db,
        "MATCH (a:A) OPTIONAL MATCH (a)-[r:R]->() WITH a WHERE r IS NULL RETURN a.name",
    );
    assert_eq!(sorted_strings(&rows, "a.name"), vec!["a2"]);
}

// ── D3: WITH WHERE uses the projected value ────────────────────────────────

#[test]
fn d3_with_where_does_not_reevaluate_alias() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND range(1, 2000) AS i WITH rand() AS r WHERE r < 0.1 \
         RETURN max(r) AS m, count(*) AS c",
    );
    let m = match rows[0].get("m") {
        Some(Value::F64(m)) => *m,
        other => panic!("expected float max, got {other:?}"),
    };
    assert!(m < 0.1, "filtered value leaked through WHERE: {m}");
    assert!(ints(&rows, "c")[0] > 0);
}

#[test]
fn d3_with_where_mixed_conjuncts_use_projected_value() {
    // `i` is dropped by the WITH, so `i > 1000` runs before projection; the
    // `r < 0.1` conjunct must still test the projected value.
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "UNWIND range(1, 2000) AS i WITH rand() AS r WHERE r < 0.1 AND i > 1000 \
         RETURN max(r) AS m, count(*) AS c",
    );
    let m = match rows[0].get("m") {
        Some(Value::F64(m)) => *m,
        other => panic!("expected float max, got {other:?}"),
    };
    assert!(m < 0.1, "filtered value leaked through WHERE: {m}");
    let c = ints(&rows, "c")[0];
    assert!(c > 0 && c <= 1000, "i > 1000 must still filter: {c}");
}

// ── D4: anonymous relationships in named paths get unique aliases ──────────

fn two_r_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:A)-[:R {k: 1}]->(:B), (:A)-[:R {k: 2}]->(:B)",
    );
    db
}

#[test]
fn d4_named_paths_in_separate_matches_are_independent() {
    let mut db = two_r_db();
    assert_eq!(
        int(
            &mut db,
            "MATCH p = ()-[:R]->() MATCH q = ()-[:R]->() RETURN count(*)"
        ),
        4
    );
}

#[test]
fn d4_named_paths_in_one_match_enforce_rel_uniqueness() {
    let mut db = two_r_db();
    assert_eq!(
        int(
            &mut db,
            "MATCH p = ()-[:R]->(), q = ()-[:R]->() RETURN count(*)"
        ),
        2
    );
}

#[test]
fn d4_named_path_relationships_resolve() {
    let mut db = two_r_db();
    let rows = run(
        &mut db,
        "MATCH p = ()-[:R]->() RETURN [r IN relationships(p) | r.k] AS ks ORDER BY ks",
    );
    assert_eq!(
        col(&rows, "ks"),
        vec![
            Value::List(vec![Value::I64(1)]),
            Value::List(vec![Value::I64(2)])
        ]
    );
}

// ── D5: read-only UNWIND ... MATCH ... RETURN ──────────────────────────────

fn id_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:N {id: 1}), (:N {id: 2}), (:N {id: 3})");
    db
}

#[test]
fn d5_unwind_then_match() {
    let mut db = id_db();
    let rows = run(&mut db, "UNWIND [1, 2] AS v MATCH (n) RETURN v");
    let mut vs = ints(&rows, "v");
    vs.sort();
    assert_eq!(vs, vec![1, 1, 1, 2, 2, 2]);
}

#[test]
fn d5_unwind_param_then_match_inline_property() {
    let mut db = id_db();
    let rows = run_p(
        &mut db,
        "UNWIND $ids AS id MATCH (n {id: id}) RETURN n.id ORDER BY n.id",
        &[("ids", Value::List(vec![Value::I64(1), Value::I64(3)]))],
    );
    assert_eq!(ints(&rows, "n.id"), vec![1, 3]);
}

#[test]
fn d5_unwind_then_match_where() {
    let mut db = id_db();
    let rows = run(
        &mut db,
        "UNWIND [1, 2, 3] AS id MATCH (n:N) WHERE n.id = id AND id > 1 RETURN n.id ORDER BY n.id",
    );
    assert_eq!(ints(&rows, "n.id"), vec![2, 3]);
}

#[test]
fn d5_unwind_then_optional_match() {
    let mut db = id_db();
    let rows = run(
        &mut db,
        "UNWIND [1, 9] AS id OPTIONAL MATCH (n:N {id: id}) RETURN id, n.id ORDER BY id",
    );
    assert_eq!(ints(&rows, "id"), vec![1, 9]);
    assert_eq!(col(&rows, "n.id"), vec![Value::I64(1), Value::Null]);
    let rows = run(
        &mut db,
        "UNWIND [1, 2] AS id OPTIONAL MATCH (n:N) WHERE n.id = id + 1 RETURN id, n.id ORDER BY id",
    );
    assert_eq!(col(&rows, "n.id"), vec![Value::I64(2), Value::I64(3)]);
}

#[test]
fn d5_unwind_match_then_with() {
    let mut db = id_db();
    let rows = run(
        &mut db,
        "UNWIND [1, 2] AS id MATCH (n:N {id: id}) WITH n.id * 10 AS t RETURN t ORDER BY t",
    );
    assert_eq!(ints(&rows, "t"), vec![10, 20]);
}

// ── D6: backtick identifiers and block comments in the pre-scans ───────────

#[test]
fn d6_backtick_identifier_with_quote() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(&mut db, "RETURN 1 AS `it's`");
    assert_eq!(ints(&rows, "it's"), vec![1]);
    let rows = run(&mut db, "RETURN 'a' AS `say \"hi`");
    assert_eq!(col(&rows, "say \"hi"), vec![Value::String("a".into())]);
}

#[test]
fn d6_block_comments() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(&mut db, "RETURN /* it's a ( comment */ 1 AS x");
    assert_eq!(ints(&rows, "x"), vec![1]);
    let rows = run(
        &mut db,
        "/* leading\n multi-line */ UNWIND [1, 2] AS x /* mid */ RETURN x /* trailing */",
    );
    assert_eq!(ints(&rows, "x"), vec![1, 2]);
    // Comment markers inside a string are literal text.
    let rows = run(&mut db, "RETURN '/* not */ // either' AS s");
    assert_eq!(
        col(&rows, "s"),
        vec![Value::String("/* not */ // either".into())]
    );
}

#[test]
fn d6_brackets_in_comments_do_not_count_toward_depth() {
    let mut db = Database::open_memory().unwrap();
    let parens = "(".repeat(300);
    let q = format!("RETURN /* {parens} */ 1 AS x // {parens}\n");
    assert_eq!(ints(&run(&mut db, &q), "x"), vec![1]);
    let q = format!("RETURN 1 AS `{parens}`");
    assert_eq!(ints(&run(&mut db, &q), &parens), vec![1]);
}

#[test]
fn d6_unclosed_string_still_reported() {
    let mut db = Database::open_memory().unwrap();
    match db.execute("RETURN `ok` + 'abc") {
        Err(GraphError::Query(qe)) => {
            assert_eq!(qe.code(), ErrorCode::UnexpectedSyntax);
            assert!(
                qe.message()
                    .contains("unclosed single-quoted string literal"),
                "{qe}"
            );
        }
        other => panic!("expected unclosed-string error, got {other:?}"),
    }
}

#[test]
fn d6_unclosed_block_comment_is_syntax_error() {
    let mut db = Database::open_memory().unwrap();
    expect_code(
        &mut db,
        "RETURN 1 AS x /* never closed",
        ErrorCode::UnexpectedSyntax,
    );
}

// ── D7 / D8: FTS rewrite guards ────────────────────────────────────────────

fn fts_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:D {t: 'abcdef', u: 'bcd'}), (:D {t: 'xyz123', u: 'xyz'}), \
         (:D {t: 'qqqqq', u: 'zzz'})",
    );
    db.begin_write().unwrap();
    db.create_fulltext_index("D", "t").unwrap();
    db.commit().unwrap();
    db
}

#[test]
fn d7_fts_term_referencing_alias_not_rewritten() {
    let mut db = fts_db();
    let q = "MATCH (n:D) WHERE n.t CONTAINS n.u RETURN n.t ORDER BY n.t";
    let plan = explain(&mut db, q);
    assert!(!plan.contains("FullTextLookup"), "{plan}");
    assert_eq!(
        sorted_strings(&run(&mut db, q), "n.t"),
        vec!["abcdef", "xyz123"]
    );
    let q = "MATCH (n:D) WHERE n.t STARTS WITH n.u RETURN n.t";
    assert_eq!(sorted_strings(&run(&mut db, q), "n.t"), vec!["xyz123"]);
}

#[test]
fn d7_fts_or_chain_with_alias_term_not_rewritten() {
    let mut db = fts_db();
    let q = "MATCH (n:D) WHERE n.t CONTAINS n.u OR n.t CONTAINS 'qqq' RETURN n.t";
    assert_eq!(
        sorted_strings(&run(&mut db, q), "n.t"),
        vec!["abcdef", "qqqqq", "xyz123"]
    );
}

#[test]
fn d7_fts_literal_term_still_rewritten() {
    let mut db = fts_db();
    let q = "MATCH (n:D) WHERE n.t CONTAINS 'cde' RETURN n.t";
    let plan = explain(&mut db, q);
    assert!(plan.contains("FullTextLookup :D.t AS n"), "{plan}");
    assert_eq!(sorted_strings(&run(&mut db, q), "n.t"), vec!["abcdef"]);
}

#[test]
fn d8_fts_non_string_literal_term_matches_nothing() {
    let mut db = fts_db();
    assert_eq!(
        int(
            &mut db,
            "MATCH (n:D) WHERE n.t CONTAINS 123 RETURN count(n)"
        ),
        0
    );
    assert_eq!(
        int(
            &mut db,
            "MATCH (n:D) WHERE n.t STARTS WITH true RETURN count(n)"
        ),
        0
    );
    let plan = explain(&mut db, "MATCH (n:D) WHERE n.t CONTAINS 123 RETURN n");
    assert!(!plan.contains("FullTextLookup"), "{plan}");
}

#[test]
fn d8_fts_non_string_param_term_matches_nothing() {
    let mut db = fts_db();
    let rows = run_p(
        &mut db,
        "MATCH (n:D) WHERE n.t CONTAINS $x RETURN count(n) AS c",
        &[("x", Value::I64(123))],
    );
    assert_eq!(ints(&rows, "c"), vec![0]);
    let rows = run_p(
        &mut db,
        "MATCH (n:D) WHERE n.t CONTAINS $x RETURN count(n) AS c",
        &[("x", Value::String("123".into()))],
    );
    assert_eq!(ints(&rows, "c"), vec![1]);
}

// ── D9: non-literal SKIP/LIMIT is not frozen by the plan cache ─────────────

#[test]
fn d9_random_limit_is_reevaluated_per_execution() {
    let mut db = Database::open_memory().unwrap();
    let q = "UNWIND range(1, 100) AS i RETURN i LIMIT toInteger(rand() * 50) + 1";
    let mut counts = std::collections::HashSet::new();
    for _ in 0..20 {
        counts.insert(run(&mut db, q).len());
    }
    assert!(counts.len() > 1, "LIMIT frozen at {counts:?}");
}

#[test]
fn d9_literal_limit_still_works_repeatedly() {
    let mut db = Database::open_memory().unwrap();
    for _ in 0..3 {
        let rows = run(&mut db, "UNWIND range(1, 10) AS i RETURN i SKIP 2 LIMIT 3");
        assert_eq!(ints(&rows, "i"), vec![3, 4, 5]);
    }
}

// ── ORDER BY scope after WITH ───────────────────────────────────────────────

#[test]
fn return_order_by_rejects_variable_dropped_by_with() {
    let mut db = v_db();
    expect_code(
        &mut db,
        "MATCH (n) WITH n.v AS v RETURN v ORDER BY n.w",
        ErrorCode::UndefinedVariable,
    );
    // Projected names (and RETURN aliases) stay sortable.
    let rows = run(
        &mut db,
        "MATCH (n) WITH n.v AS v RETURN v AS x ORDER BY x DESC",
    );
    assert_eq!(ints(&rows, "x"), vec![3, 2, 1]);
    let rows = run(&mut db, "MATCH (n) WITH n RETURN n.v ORDER BY n.w");
    assert_eq!(ints(&rows, "n.v"), vec![3, 1, 2]);
}
