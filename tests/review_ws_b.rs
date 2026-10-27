//! Regression tests for workstream B of the October 2026 code review:
//! relationship identity and the read engines (materialized, pull-based,
//! slot and correlated). Test names carry the finding id (B1..B14, which map
//! to the review's table/appendix ids noted on each section) and every test
//! fails on the pre-fix code.

use graphdblite::{Database, Record, Value};

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn db_with(setup: &[&str]) -> Database {
    let mut db = Database::open_memory().unwrap();
    for q in setup {
        run(&mut db, q);
    }
    db
}

fn single(db: &mut Database, q: &str) -> Value {
    let rows = run(db, q);
    assert_eq!(rows.len(), 1, "expected one row from {q}, got {rows:?}");
    let v = rows[0].values().next().cloned();
    v.expect("no columns")
}

fn int(db: &mut Database, q: &str) -> i64 {
    match single(db, q) {
        Value::I64(n) => n,
        other => panic!("expected integer from {q}, got {other:?}"),
    }
}

/// Sorted integer column; nulls map to `None`.
fn ints(db: &mut Database, q: &str) -> Vec<Option<i64>> {
    let mut out: Vec<Option<i64>> = run(db, q)
        .iter()
        .map(|r| match r.values().next() {
            Some(Value::I64(n)) => Some(*n),
            Some(Value::Null) => None,
            other => panic!("expected integer or null from {q}, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

/// Sorted string column.
fn strs(db: &mut Database, q: &str) -> Vec<String> {
    let mut out: Vec<String> = run(db, q)
        .iter()
        .map(|r| match r.values().next() {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected string from {q}, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn create_index(db: &mut Database, label: &str, prop: &str) {
    db.begin_write().unwrap();
    db.create_index(label, prop).unwrap();
    db.commit().unwrap();
}

fn create_fts(db: &mut Database, label: &str, prop: &str) {
    db.begin_write().unwrap();
    db.create_fulltext_index(label, prop).unwrap();
    db.commit().unwrap();
}

const MUTUAL: &str = "CREATE (a:A {name:'a'})-[:R {w:1}]->(b:B {name:'b'}), (b)-[:R {w:3}]->(a)";
const PARALLEL: &str = "CREATE (a:A {name:'a'})-[:R {w:1}]->(b:B {name:'b'}), (a)-[:R {w:2}]->(b)";

// ── B1 (#1): undirected match keeps both edges of a mutual pair ───────────

#[test]
fn b1_undirected_typed_returns_both_orientations() {
    let mut db = db_with(&[MUTUAL]);
    assert_eq!(
        ints(&mut db, "MATCH (a:A)-[r:R]-(b) RETURN r.w"),
        vec![Some(1), Some(3)]
    );
}

#[test]
fn b1_undirected_untyped_returns_both_orientations() {
    let mut db = db_with(&[MUTUAL]);
    assert_eq!(
        ints(&mut db, "MATCH (a:A)-[r]-(b) RETURN r.w"),
        vec![Some(1), Some(3)]
    );
}

#[test]
fn b1_undirected_with_parallel_edges_each_way() {
    let mut db = db_with(&[MUTUAL, "MATCH (a:A), (b:B) CREATE (a)-[:R {w:2}]->(b)"]);
    assert_eq!(
        ints(&mut db, "MATCH (a:A)-[r:R]-(b) RETURN r.w"),
        vec![Some(1), Some(2), Some(3)]
    );
}

#[test]
fn b1_undirected_correlated_returns_both_orientations() {
    let mut db = db_with(&[MUTUAL]);
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:A) OPTIONAL MATCH (a)-[r:R]-(b) RETURN r.w"
        ),
        vec![Some(1), Some(3)]
    );
    assert_eq!(
        ints(&mut db, "MATCH (a:A) MATCH (a)-[r:R]-(b) RETURN r.w"),
        vec![Some(1), Some(3)]
    );
}

#[test]
fn b1_undirected_self_loop_is_returned_once() {
    let mut db = db_with(&["CREATE (s:S)-[:L {w:5}]->(s)"]);
    assert_eq!(
        ints(&mut db, "MATCH (s:S)-[r:L]-(x) RETURN r.w"),
        vec![Some(5)]
    );
    assert_eq!(
        ints(&mut db, "MATCH (s:S) MATCH (s)-[r:L]-(x) RETURN r.w"),
        vec![Some(5)]
    );
}

// ── B2 (#8, #10 edges, appendix S2b): relationship values keep identity ───

const TWIN: &str = "CREATE (a:A)-[:R]->(b:B), (a)-[:R]->(b)";

#[test]
fn b2_count_distinct_parallel_identical_edges() {
    let mut db = db_with(&[TWIN]);
    assert_eq!(
        int(&mut db, "MATCH (:A)-[r:R]->(:B) RETURN count(DISTINCT r)"),
        2
    );
}

#[test]
fn b2_parallel_edge_values_are_not_equal() {
    let mut db = db_with(&[TWIN]);
    assert_eq!(
        single(
            &mut db,
            "MATCH (:A)-[r:R]->(:B) WITH collect(r) AS rs RETURN rs[0] = rs[1]"
        ),
        Value::Bool(false)
    );
    // Same edge compared with itself stays equal.
    assert_eq!(
        single(
            &mut db,
            "MATCH (:A)-[r:R]->(:B) WITH collect(r) AS rs RETURN rs[0] = rs[0]"
        ),
        Value::Bool(true)
    );
}

#[test]
fn b2_var_length_edge_values_are_distinct() {
    let mut db = db_with(&[TWIN]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (:A)-[rs:R*1]->(:B) UNWIND rs AS r RETURN count(DISTINCT r)"
        ),
        2
    );
}

#[test]
fn b2_returned_edges_expose_distinct_seq() {
    let mut db = db_with(&[TWIN]);
    let mut seqs: Vec<u64> = run(&mut db, "MATCH (:A)-[r:R]->(:B) RETURN r")
        .iter()
        .map(|r| match r.get("r") {
            Some(Value::Edge(e)) => e.seq,
            other => panic!("expected edge, got {other:?}"),
        })
        .collect();
    seqs.sort();
    assert_eq!(seqs.len(), 2);
    assert_ne!(seqs[0], seqs[1]);
}

#[test]
fn b2_delete_path_relationship_value_deletes_only_that_edge() {
    let mut db = db_with(&["CREATE (a:A)-[:R {k:1}]->(b:B), (a)-[:R {k:2}]->(b)"]);
    run(
        &mut db,
        "MATCH p=(:A)-[r:R {k:2}]->(:B) UNWIND relationships(p) AS x DELETE x",
    );
    assert_eq!(
        ints(&mut db, "MATCH ()-[r:R]->() RETURN r.k"),
        vec![Some(1)]
    );
}

#[test]
fn b2_delete_collected_relationship_deletes_only_that_edge() {
    let mut db = db_with(&["CREATE (a:A)-[:R {k:1}]->(b:B), (a)-[:R {k:2}]->(b)"]);
    run(
        &mut db,
        "MATCH (:A)-[r:R {k:2}]->(:B) WITH collect(r) AS rs UNWIND rs AS x DELETE x",
    );
    assert_eq!(
        ints(&mut db, "MATCH ()-[r:R]->() RETURN r.k"),
        vec![Some(1)]
    );
}

#[test]
fn b2_delete_relationship_expression_deletes_only_that_edge() {
    let mut db = db_with(&["CREATE (a:A)-[:R {k:1}]->(b:B), (a)-[:R {k:2}]->(b)"]);
    run(
        &mut db,
        "MATCH (:A)-[r:R {k:2}]->(:B) WITH collect(r) AS rs DELETE rs[0]",
    );
    assert_eq!(
        ints(&mut db, "MATCH ()-[r:R]->() RETURN r.k"),
        vec![Some(1)]
    );
}

#[test]
fn b2_set_on_collected_relationship_targets_that_edge() {
    let mut db = db_with(&["CREATE (a:A)-[:R {k:1}]->(b:B), (a)-[:R {k:2}]->(b)"]);
    run(
        &mut db,
        "MATCH (:A)-[r:R {k:2}]->(:B) WITH head(collect(r)) AS x SET x.z = 9",
    );
    let rows = run(
        &mut db,
        "MATCH ()-[r:R]->() RETURN r.k AS k, r.z AS z ORDER BY k",
    );
    let got: Vec<(Value, Value)> = rows
        .iter()
        .map(|r| (r.get("k").cloned().unwrap(), r.get("z").cloned().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![(Value::I64(1), Value::Null), (Value::I64(2), Value::I64(9))]
    );
}

#[test]
fn b2_set_on_collected_node_targets_that_node() {
    let mut db = db_with(&["CREATE (:P {k:1}), (:P {k:2})"]);
    run(
        &mut db,
        "MATCH (n:P {k:2}) WITH head(collect(n)) AS m SET m.y = 7",
    );
    let rows = run(&mut db, "MATCH (n:P) RETURN n.k AS k, n.y AS y ORDER BY k");
    let got: Vec<(Value, Value)> = rows
        .iter()
        .map(|r| (r.get("k").cloned().unwrap(), r.get("y").cloned().unwrap()))
        .collect();
    assert_eq!(
        got,
        vec![(Value::I64(1), Value::Null), (Value::I64(2), Value::I64(7))]
    );
}

// ── B3 (#6): correlated single-hop Expand keeps parallel edges ────────────

#[test]
fn b3_optional_match_returns_every_parallel_edge() {
    let mut db = db_with(&[PARALLEL]);
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:A) OPTIONAL MATCH (a)-[r:R]->(b) RETURN r.w"
        ),
        vec![Some(1), Some(2)]
    );
}

#[test]
fn b3_second_match_counts_every_parallel_edge() {
    let mut db = db_with(&[PARALLEL]);
    assert_eq!(
        int(&mut db, "MATCH (a:A) MATCH (a)-[r:R]->(b) RETURN count(r)"),
        2
    );
}

#[test]
fn b3_pattern_comprehension_sees_every_parallel_edge() {
    let mut db = db_with(&[PARALLEL]);
    let v = single(&mut db, "MATCH (a:A) RETURN [(a)-[r:R]->(b) | r.w] AS ws");
    let Value::List(mut items) = v else {
        panic!("expected list, got {v:?}")
    };
    items.sort_by_key(|v| match v {
        Value::I64(n) => *n,
        _ => i64::MAX,
    });
    assert_eq!(items, vec![Value::I64(1), Value::I64(2)]);
}

#[test]
fn b3_comma_pattern_pairs_distinct_parallel_edges() {
    let mut db = db_with(&[PARALLEL]);
    let rows = run(
        &mut db,
        "MATCH (a:A) MATCH (a)-[r1:R]->(b), (a)-[r2:R]->(b) RETURN r1.w AS x, r2.w AS y",
    );
    let mut got: Vec<(Value, Value)> = rows
        .iter()
        .map(|r| (r.get("x").cloned().unwrap(), r.get("y").cloned().unwrap()))
        .collect();
    got.sort_by_key(|(x, _)| match x {
        Value::I64(n) => *n,
        _ => 0,
    });
    assert_eq!(
        got,
        vec![
            (Value::I64(1), Value::I64(2)),
            (Value::I64(2), Value::I64(1))
        ]
    );
}

#[test]
fn b3_correlated_rel_property_is_per_edge() {
    let mut db = db_with(&[PARALLEL]);
    assert_eq!(
        ints(&mut db, "MATCH (a:A) MATCH (a)-[r:R {w:2}]->(b) RETURN r.w"),
        vec![Some(2)]
    );
}

#[test]
fn b3_rebound_relationship_matches_only_that_parallel_edge() {
    let mut db = db_with(&[PARALLEL]);
    assert_eq!(
        ints(
            &mut db,
            "MATCH ()-[r:R {w:2}]->() MATCH (a)-[r]->(b) RETURN r.w"
        ),
        vec![Some(2)]
    );
    // Undirected: the bound edge matches once from each endpoint.
    assert_eq!(
        int(
            &mut db,
            "MATCH ()-[r:R {w:2}]->() MATCH (x)-[r]-(y) RETURN count(*)"
        ),
        2
    );
    // Wrong direction: no match.
    assert_eq!(
        int(
            &mut db,
            "MATCH ()-[r:R {w:2}]->() MATCH (a:A)<-[r]-(b) RETURN count(*)"
        ),
        0
    );
}

// ── B4 (#7, planner P5): cross-segment relationship uniqueness ────────────

const CYCLE: &str = "CREATE (a:A {name:'a'})-[:R {w:1}]->(b:B {name:'b'}), (b)-[:R {w:2}]->(a)";

#[test]
fn b4_fixed_then_var_length_does_not_reuse_edge() {
    let mut db = db_with(&[CYCLE]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A)-[r1:R]->(b)-[rs:R*1..2]-(x) RETURN count(*)"
        ),
        1
    );
}

#[test]
fn b4_fixed_then_var_length_correlated() {
    let mut db = db_with(&[CYCLE]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A) MATCH (a)-[r1:R]->(b)-[rs:R*1..2]-(x) RETURN count(*)"
        ),
        1
    );
}

#[test]
fn b4_fixed_then_var_length_read_only_return_rows() {
    let mut db = db_with(&[CYCLE]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A)-[r1:R]->(b)-[rs:R*1..2]-(x) RETURN x.name"
        ),
        vec!["a".to_string()]
    );
}

#[test]
fn b4_anonymous_fixed_hop_is_not_reused() {
    let mut db = db_with(&["CREATE (a:S {name:'a'})-[:R]->(m:M)"]);
    assert_eq!(
        int(&mut db, "MATCH (a:S)-[:R]->(m)-[*1..1]-(x) RETURN count(*)"),
        0
    );
}

#[test]
fn b4_single_hop_segments_do_not_reuse_edge() {
    let mut db = db_with(&["CREATE (a:A)-[:R]->(b:B)"]);
    let q = "MATCH (a:A)-[r1:R]->(b)<-[r2:R]-(a) RETURN count(*)";
    assert_eq!(int(&mut db, q), 0);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A) MATCH (a)-[r1:R]->(b)<-[r2:R]-(a) RETURN count(*)"
        ),
        0
    );
    // With a parallel twin each edge can pair with the other one.
    run(&mut db, "MATCH (a:A), (b:B) CREATE (a)-[:R]->(b)");
    assert_eq!(int(&mut db, q), 2);
}

#[test]
fn b4_uniqueness_is_directed_not_endpoint_pair() {
    // a->b and b->a are different relationships: both may appear in one path.
    let mut db = db_with(&[CYCLE]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A)-[r1:R]->(b)-[r2:R]->(c) RETURN count(*)"
        ),
        1
    );
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A) MATCH (a)-[r1:R]->(b)-[r2:R]->(c) RETURN count(*)"
        ),
        1
    );
}

#[test]
fn b4_limit_pushdown_keeps_uniqueness() {
    let mut db = db_with(&[CYCLE]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A)-[r1:R]->(b)-[rs:R*1..2]-(x) RETURN x.name LIMIT 5"
        ),
        vec!["a".to_string()]
    );
}

// ── B5 (#12): IndexLookup / FullTextLookup residuals see the outer row ────

#[test]
fn b5_index_lookup_residual_uses_outer_row() {
    let mut db = db_with(&["CREATE (:L {k:1, name:'a'}), (:L {k:1, name:'b'}), (:M {nm:'a'})"]);
    let q = "MATCH (m:M) MATCH (n:L {k:1, name: m.nm}) RETURN n.name";
    assert_eq!(strs(&mut db, q), vec!["a".to_string()]);
    create_index(&mut db, "L", "k");
    assert_eq!(strs(&mut db, q), vec!["a".to_string()]);
}

#[test]
fn b5_fulltext_lookup_residual_uses_outer_row() {
    let mut db = db_with(&["CREATE (:Q {z:1}), (:L {t:'apple', k:1}), (:L {t:'applet', k:2})"]);
    let q = "MATCH (q:Q) MATCH (n:L) WHERE n.t CONTAINS 'app' AND n.k = q.z RETURN n.t";
    assert_eq!(strs(&mut db, q), vec!["apple".to_string()]);
    create_fts(&mut db, "L", "t");
    assert_eq!(strs(&mut db, q), vec!["apple".to_string()]);
}

// ── B6 (#13): pattern predicates after an aggregating WITH ────────────────

const ONE_LINKED: &str = "CREATE (a:A {name:'a'})-[:R]->(:B), (:A {name:'z'})";

#[test]
fn b6_pattern_predicate_after_aggregating_with() {
    let mut db = db_with(&[ONE_LINKED]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WITH a, count(*) AS c WHERE (a)-[:R]->() RETURN a.name"
        ),
        vec!["a".to_string()]
    );
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WITH a, count(*) AS c WHERE EXISTS { (a)-[:R]->() } RETURN a.name"
        ),
        vec!["a".to_string()]
    );
}

// ── B7 (#14): exists(pattern) returns the pattern's boolean ───────────────

#[test]
fn b7_exists_function_on_pattern() {
    let mut db = db_with(&[ONE_LINKED]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WHERE exists((a)-[:R]->()) RETURN a.name"
        ),
        vec!["a".to_string()]
    );
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WHERE NOT exists((a)-[:R]->()) RETURN a.name"
        ),
        vec!["z".to_string()]
    );
}

#[test]
fn b7_exists_function_on_property_still_works() {
    let mut db = db_with(&["CREATE (:A {name:'a', p:1}), (:A {name:'z'})"]);
    assert_eq!(
        strs(&mut db, "MATCH (a:A) WHERE exists(a.p) RETURN a.name"),
        vec!["a".to_string()]
    );
}

// ── B8 (#15): correlated MaterializePath ───────────────────────────────────

#[test]
fn b8_correlated_var_length_path_nodes_follow_direction() {
    let mut db = db_with(&[
        "CREATE (b:B {name:'b'}), (x:X {name:'x'}), (c:C {name:'c'}), (x)-[:R]->(b), (c)-[:R]->(x)",
    ]);
    let want = Value::List(vec![
        Value::String("b".into()),
        Value::String("x".into()),
        Value::String("c".into()),
    ]);
    assert_eq!(
        single(
            &mut db,
            "MATCH (b:B) OPTIONAL MATCH p=(b)<-[:R*2]-(y) RETURN [n IN nodes(p) | n.name]"
        ),
        want
    );
    assert_eq!(
        single(
            &mut db,
            "MATCH (b:B) MATCH p=(b)<-[:R*2]-(y) RETURN [n IN nodes(p) | n.name]"
        ),
        want
    );
}

#[test]
fn b8_correlated_path_uniqueness_respects_edge_seq() {
    let mut db = db_with(&[TWIN]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A) MATCH p=(a)-[r:R]->(b)<-[rs:R*1]-(c) RETURN count(p)"
        ),
        2
    );
}

// ── B9 (#16): EXISTS {} sees outer variables ──────────────────────────────

const EXISTS_SETUP: &str = "CREATE (:M {k:1, q:'yes'}), (:M {k:2, q:'no'}), (:L {k:1})";

#[test]
fn b9_exists_subquery_where_references_outer() {
    let mut db = db_with(&[EXISTS_SETUP]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (m:M) WHERE EXISTS { MATCH (n:L) WHERE n.k = m.k } RETURN m.q"
        ),
        vec!["yes".to_string()]
    );
}

#[test]
fn b9_exists_pattern_where_references_outer() {
    let mut db = db_with(&[EXISTS_SETUP]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (m:M) WHERE EXISTS { (n:L) WHERE n.k = m.k } RETURN m.q"
        ),
        vec!["yes".to_string()]
    );
}

#[test]
fn b9_exists_subquery_still_rejects_unknown_variable() {
    let mut db = db_with(&[EXISTS_SETUP]);
    let err = db
        .execute("MATCH (m:M) WHERE EXISTS { MATCH (n:L) WHERE n.k = nope.k } RETURN m.q")
        .unwrap_err();
    assert!(
        err.to_string().contains("nope"),
        "expected UndefinedVariable for nope, got {err}"
    );
    match &err {
        graphdblite::GraphError::Query(q) => {
            assert_eq!(q.code(), graphdblite::ErrorCode::UndefinedVariable)
        }
        other => panic!("expected query error, got {other:?}"),
    }
}

// ── B10 (appendix R10): EXISTS {} correlation through LIMIT etc. ──────────

#[test]
fn b10_exists_subquery_correlates_through_tail_operators() {
    let mut db = db_with(&[ONE_LINKED]);
    for tail in [
        "RETURN x LIMIT 1",
        "RETURN x SKIP 0",
        "RETURN DISTINCT x",
        "RETURN x ORDER BY x.name",
        "UNWIND [1, 2] AS i RETURN i",
    ] {
        let q = format!("MATCH (a:A) WHERE EXISTS {{ MATCH (a)-[:R]->(x) {tail} }} RETURN a.name");
        assert_eq!(strs(&mut db, &q), vec!["a".to_string()], "{q}");
    }
}

// ── B11 (appendix R2, R3, I3, I4): correlated lookups ─────────────────────

#[test]
fn b11_prebound_id_lookup_checks_id() {
    let mut db = db_with(&["CREATE (:L {k:1}), (:L {k:2})"]);
    let other = int(&mut db, "MATCH (n:L {k:2}) RETURN id(n)");
    assert_eq!(
        int(
            &mut db,
            &format!("MATCH (n:L {{k:1}}) MATCH (n) WHERE id(n) = {other} RETURN count(*)")
        ),
        0
    );
    assert_eq!(
        int(
            &mut db,
            &format!("MATCH (n:L {{k:2}}) MATCH (n) WHERE id(n) = {other} RETURN count(*)")
        ),
        1
    );
}

#[test]
fn b11_prebound_fulltext_lookup_respects_binding() {
    let mut db = db_with(&["CREATE (:L {t:'apple'}), (:L {t:'apply'})"]);
    create_fts(&mut db, "L", "t");
    assert_eq!(
        strs(
            &mut db,
            "MATCH (n:L {t:'apple'}) MATCH (n) WHERE n.t CONTAINS 'appl' RETURN n.t"
        ),
        vec!["apple".to_string()]
    );
    assert_eq!(
        strs(
            &mut db,
            "MATCH (n:L {t:'apple'}) MATCH (n:L) WHERE n.t CONTAINS 'appl' RETURN n.t"
        ),
        vec!["apple".to_string()]
    );
}

#[test]
fn b11_prebound_index_lookup_uses_cypher_equality() {
    let mut db = db_with(&["CREATE (:L {k:1})"]);
    create_index(&mut db, "L", "k");
    assert_eq!(
        int(&mut db, "MATCH (n:L) MATCH (n:L {k: 1.0}) RETURN count(*)"),
        1
    );
    assert_eq!(
        int(&mut db, "MATCH (n:L) MATCH (n:L {k: 1.5}) RETURN count(*)"),
        0
    ); // A second indexed node must not leak into the pre-bound row.
    run(&mut db, "MATCH (n:L) SET n.name = 'x'");
    run(&mut db, "CREATE (:L {k:1, name:'y'})");
    assert_eq!(
        strs(
            &mut db,
            "MATCH (n:L {name:'x'}) MATCH (n:L {k:1}) RETURN n.name"
        ),
        vec!["x".to_string()]
    );
}

#[test]
fn b11_id_lookup_accepts_whole_number_float() {
    let mut db = db_with(&["CREATE (:L {k:1}), (:L {k:2})"]);
    let id = int(&mut db, "MATCH (n:L {k:2}) RETURN id(n)");
    assert_eq!(
        ints(
            &mut db,
            &format!("MATCH (n) WHERE id(n) = {id}.0 RETURN n.k")
        ),
        vec![Some(2)]
    );
    assert_eq!(
        ints(
            &mut db,
            &format!("MATCH (n) WHERE id(n) = {id}.5 RETURN n.k")
        ),
        Vec::<Option<i64>>::new()
    );
}

// ── B12 (appendix I1): pattern predicates honour var-length property maps ──

#[test]
fn b12_pattern_predicate_var_length_property_map() {
    let mut db = db_with(&["CREATE (a:A {name:'a'})-[:R {w:1}]->(b:B)-[:R {w:2}]->(c:C)"]);
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WHERE (a)-[:R*2 {w:1}]->() RETURN a.name"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        strs(
            &mut db,
            "MATCH (a:A) WHERE (a)-[:R*1 {w:1}]->() RETURN a.name"
        ),
        vec!["a".to_string()]
    );
}

// ── B13 (appendix S4): var-length property maps use Cypher equality ───────

#[test]
fn b13_var_length_property_filter_numeric_equality() {
    let mut db = db_with(&["CREATE (a:A)-[:R {w:1}]->(b:B)"]);
    assert_eq!(
        int(&mut db, "MATCH (a:A)-[:R*1 {w:1.0}]->(b) RETURN count(*)"),
        1
    );
    assert_eq!(
        int(&mut db, "MATCH (a:A)-[:R*1 {w:'1'}]->(b) RETURN count(*)"),
        0
    );
    assert_eq!(
        int(&mut db, "MATCH (a:A)-[:R*1 {w:1.5}]->(b) RETURN count(*)"),
        0
    );
}

// ── B14 (appendix R11): LIMIT pushdown with a bound var-length destination ─

#[test]
fn b14_limit_pushdown_with_bound_destination_keeps_row() {
    let mut db = db_with(&["CREATE (a:A)-[:R]->(b:B {name:'b'}), (b)-[:R]->(d:D), (d)-[:R]->(a)"]);
    run(
        &mut db,
        "MATCH (a:A)-[:R]->(b)-[:R*1..3]->(a) WITH b LIMIT 1 SET b.hit = true",
    );
    assert_eq!(
        strs(&mut db, "MATCH (n {hit: true}) RETURN n.name"),
        vec!["b".to_string()]
    );
}
