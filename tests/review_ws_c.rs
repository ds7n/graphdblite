//! Regression tests for workstream C of the October 2026 code review:
//! writes and MERGE. Test names carry the finding id (C1..C13, which map to
//! the review's table/appendix ids noted on each section) and every test
//! fails on the pre-fix code.

use graphdblite::{Database, ErrorCode, GraphError, Properties, Record, Value};

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

/// First column of every row, in row order (integers or nulls).
fn ints_in_order(db: &mut Database, q: &str) -> Vec<Option<i64>> {
    run(db, q)
        .iter()
        .map(|r| match r.values().next() {
            Some(Value::I64(n)) => Some(*n),
            Some(Value::Null) => None,
            other => panic!("expected integer or null from {q}, got {other:?}"),
        })
        .collect()
}

/// Sorted first column (integers or nulls).
fn ints(db: &mut Database, q: &str) -> Vec<Option<i64>> {
    let mut out = ints_in_order(db, q);
    out.sort();
    out
}

fn err(db: &mut Database, q: &str) -> GraphError {
    match db.execute(q) {
        Ok(rows) => panic!("expected an error from {q}, got {rows:?}"),
        Err(e) => e,
    }
}

/// Assert `q` fails with a query error of `kind` (e.g. "TypeError") and `code`.
fn assert_query_err(db: &mut Database, q: &str, kind: &str, code: ErrorCode) {
    match err(db, q) {
        GraphError::Query(qe) => {
            assert_eq!(qe.kind(), kind, "wrong kind for {q}: {qe}");
            assert_eq!(qe.code(), code, "wrong code for {q}: {qe}");
        }
        other => panic!("expected a query error from {q}, got {other:?}"),
    }
}

fn props(pairs: &[(&str, Value)]) -> Properties {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn edge_count(db: &mut Database) -> i64 {
    int(db, "MATCH ()-[r]->() RETURN count(r)")
}

fn node_count(db: &mut Database, label: &str) -> i64 {
    int(db, &format!("MATCH (n:{label}) RETURN count(n)"))
}

// ── C1 (#2): MERGE creates the unbound parts of a relationship pattern ────

#[test]
fn c1_match_merge_creates_unbound_target() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:A) MERGE (a)-[:R]->(c:C {id:3}) RETURN c.id"
        ),
        vec![Some(3)]
    );
    assert_eq!(
        int(
            &mut db,
            "MATCH (:A {id:1})-[:R]->(c:C {id:3}) RETURN count(c)"
        ),
        1
    );
    // A second run matches the pattern instead of creating it again.
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:A) MERGE (a)-[:R]->(c:C {id:3}) RETURN c.id"
        ),
        vec![Some(3)]
    );
    assert_eq!(node_count(&mut db, "C"), 1);
    assert_eq!(edge_count(&mut db), 1);
}

#[test]
fn c1_match_merge_creates_unbound_source_reversed() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    run(&mut db, "MATCH (a:A) MERGE (c:C {id:3})-[:R]->(a)");
    assert_eq!(int(&mut db, "MATCH (:C {id:3})-[:R]->(a:A) RETURN a.id"), 1);
    run(&mut db, "MATCH (a:A) MERGE (a)<-[:S]-(d:D {id:4})");
    assert_eq!(int(&mut db, "MATCH (:D {id:4})-[:S]->(a:A) RETURN a.id"), 1);
    assert_eq!(edge_count(&mut db), 2);
    // Re-running matches the existing patterns from the bound end.
    run(&mut db, "MATCH (a:A) MERGE (c:C {id:3})-[:R]->(a)");
    run(&mut db, "MATCH (a:A) MERGE (a)<-[:S]-(d:D {id:4})");
    run(&mut db, "MATCH (a:A) MERGE (a)-[:S]-(d:D {id:4})");
    assert_eq!(edge_count(&mut db), 2);
    assert_eq!(node_count(&mut db, "C") + node_count(&mut db, "D"), 2);
}

#[test]
fn c1_match_merge_creates_new_target_when_unconnected_match_exists() {
    // openCypher MERGE is all-or-nothing on the pattern: an existing but
    // unconnected (:C {id:3}) does not satisfy (a)-[:R]->(:C {id:3}).
    let mut db = db_with(&["CREATE (:A {id:1}), (:C {id:3})"]);
    run(&mut db, "MATCH (a:A) MERGE (a)-[:R]->(c:C {id:3})");
    assert_eq!(node_count(&mut db, "C"), 2);
    assert_eq!(
        int(&mut db, "MATCH (:A)-[:R]->(c:C {id:3}) RETURN count(c)"),
        1
    );
}

#[test]
fn c1_match_merge_anonymous_target() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    run(&mut db, "MATCH (a:A) MERGE (a)-[:R]->(:C {k:3})");
    run(&mut db, "MATCH (a:A) MERGE (a)-[:R]->(:C {k:3})");
    assert_eq!(int(&mut db, "MATCH (:A)-[:R]->(c:C) RETURN c.k"), 3);
    assert_eq!(node_count(&mut db, "C"), 1);
}

#[test]
fn c1_match_merge_matches_existing_edge_to_unbound_node() {
    let mut db = db_with(&["CREATE (:A {id:1})-[:R {w:7}]->(:C {id:3})"]);
    assert_eq!(
        ints(&mut db, "MATCH (a:A) MERGE (a)-[r:R]->(c:C) RETURN r.w"),
        vec![Some(7)]
    );
    assert_eq!(node_count(&mut db, "C"), 1);
    assert_eq!(edge_count(&mut db), 1);
}

// ── C2 (#3): ON MATCH / ON CREATE can read the merged relationship ────────

#[test]
fn c2_on_match_reads_relationship_properties() {
    let mut db = db_with(&["CREATE (:A)-[:R {n:1}]->(:B)"]);
    assert_eq!(
        int(
            &mut db,
            "MATCH (a:A), (b:B) MERGE (a)-[r:R]->(b) ON MATCH SET r.n = r.n + 1 RETURN r.n"
        ),
        2
    );
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN r.n"), 2);
}

#[test]
fn c2_on_create_reads_relationship() {
    let mut db = db_with(&["CREATE (:A), (:B)"]);
    assert_eq!(
        single(
            &mut db,
            "MATCH (a:A), (b:B) MERGE (a)-[r:R {k:1}]->(b) ON CREATE SET r.t = type(r), r.k2 = r.k * 10 RETURN r.t"
        ),
        s("R")
    );
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN r.k2"), 10);
}

#[test]
fn c2_standalone_relationship_merge_counter() {
    let mut db = Database::open_memory().unwrap();
    let q = "MERGE (a:X)-[r:R]->(b:Y) ON CREATE SET r.n = 1 ON MATCH SET r.n = r.n + 1 RETURN r.n";
    assert_eq!(int(&mut db, q), 1);
    assert_eq!(int(&mut db, q), 2);
    assert_eq!(int(&mut db, q), 3);
}

// ── C3 (#5): ON CREATE / ON MATCH SET dispatch on their own variable ──────

#[test]
fn c3_relationship_merge_sets_other_variables() {
    let mut db = db_with(&["CREATE (:A), (:B)"]);
    run(
        &mut db,
        "MATCH (a:A), (b:B) MERGE (a)-[r:R]->(b) ON CREATE SET a.flag = true, a:Lab, r.w = 5, b.seen = 1",
    );
    assert_eq!(
        single(&mut db, "MATCH (a:A) RETURN a.flag"),
        Value::Bool(true)
    );
    assert_eq!(
        single(&mut db, "MATCH (a:A) RETURN labels(a)"),
        Value::List(vec![s("A"), s("Lab")])
    );
    assert_eq!(int(&mut db, "MATCH (b:B) RETURN b.seen"), 1);
    assert_eq!(
        single(&mut db, "MATCH ()-[r:R]->() RETURN properties(r)"),
        map(&[("w", Value::I64(5))])
    );
    // ON MATCH: same dispatch.
    run(
        &mut db,
        "MATCH (a:A), (b:B) MERGE (a)-[r:R]->(b) ON MATCH SET b.seen = b.seen + 1, b:Hit",
    );
    assert_eq!(int(&mut db, "MATCH (b:B:Hit) RETURN b.seen"), 2);
    assert_eq!(
        single(&mut db, "MATCH ()-[r:R]->() RETURN properties(r)"),
        map(&[("w", Value::I64(5))])
    );
}

#[test]
fn c3_node_merge_sets_other_variable() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    run(
        &mut db,
        "MATCH (a:A) MERGE (n:N {k:1}) ON CREATE SET a.touched = 1, n.made = true",
    );
    assert_eq!(int(&mut db, "MATCH (a:A) RETURN a.touched"), 1);
    assert_eq!(
        single(&mut db, "MATCH (n:N) RETURN properties(n)"),
        map(&[("k", Value::I64(1)), ("made", Value::Bool(true))])
    );
}

// ── C4 (#4): write-created aliases are node bindings ──────────────────────

fn assert_node(v: &Value, labels: &[&str], expected: Properties) {
    match v {
        Value::Node(n) => {
            assert_eq!(
                n.labels,
                labels.iter().map(|l| l.to_string()).collect::<Vec<_>>()
            );
            assert_eq!(n.properties, expected);
        }
        other => panic!("expected a node, got {other:?}"),
    }
}

#[test]
fn c4_create_return_node() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "CREATE (n:C {k:1}) RETURN n, properties(n), labels(n)",
    );
    assert_eq!(rows.len(), 1);
    assert_node(
        rows[0].get("n").unwrap(),
        &["C"],
        props(&[("k", Value::I64(1))]),
    );
    assert_eq!(
        rows[0].get("properties(n)"),
        Some(&map(&[("k", Value::I64(1))]))
    );
    assert_eq!(rows[0].get("labels(n)"), Some(&Value::List(vec![s("C")])));
}

#[test]
fn c4_create_with_return_properties() {
    let mut db = Database::open_memory().unwrap();
    assert_eq!(
        single(&mut db, "CREATE (n:C {k:1}) WITH n RETURN properties(n)"),
        map(&[("k", Value::I64(1))])
    );
}

#[test]
fn c4_merge_return_node() {
    let mut db = Database::open_memory().unwrap();
    for _ in 0..2 {
        let v = single(&mut db, "MERGE (n:M {k:2}) ON CREATE SET n.c = 1 RETURN n");
        assert_node(
            &v,
            &["M"],
            props(&[("k", Value::I64(2)), ("c", Value::I64(1))]),
        );
    }
}

#[test]
fn c4_match_create_return_new_node_properties() {
    let mut db = db_with(&["CREATE (:A)"]);
    assert_eq!(
        single(
            &mut db,
            "MATCH (a:A) CREATE (a)-[:R]->(b:D {z:1}) RETURN properties(b)"
        ),
        map(&[("z", Value::I64(1))])
    );
}

#[test]
fn c4_create_return_relationship_and_node_in_path_pattern() {
    let mut db = Database::open_memory().unwrap();
    let rows = run(
        &mut db,
        "CREATE (a:A {x:1})-[r:R {w:2}]->(b:B) RETURN a, type(r), properties(r), b",
    );
    assert_node(
        rows[0].get("a").unwrap(),
        &["A"],
        props(&[("x", Value::I64(1))]),
    );
    assert_node(rows[0].get("b").unwrap(), &["B"], Properties::new());
    assert_eq!(rows[0].get("type(r)"), Some(&s("R")));
    assert_eq!(
        rows[0].get("properties(r)"),
        Some(&map(&[("w", Value::I64(2))]))
    );
}

// ── C5 (#9): SET across rows reads the current stored value ───────────────

#[test]
fn c5_set_node_across_rows_sees_previous_writes() {
    let mut db = db_with(&["CREATE (:P {c:0})"]);
    assert_eq!(
        ints_in_order(
            &mut db,
            "MATCH (n:P) UNWIND [1,2,3] AS i SET n.c = n.c + 1 RETURN n.c"
        ),
        vec![Some(1), Some(2), Some(3)]
    );
    assert_eq!(int(&mut db, "MATCH (n:P) RETURN n.c"), 3);
}

#[test]
fn c5_set_edge_across_rows_sees_previous_writes() {
    let mut db = db_with(&["CREATE (:X)-[:R {w:0}]->(:Y)"]);
    assert_eq!(
        ints_in_order(
            &mut db,
            "MATCH ()-[r:R]->() UNWIND [1,2,3] AS i SET r.w = r.w + 1 RETURN r.w"
        ),
        vec![Some(1), Some(2), Some(3)]
    );
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN r.w"), 3);
}

#[test]
fn c5_map_merge_across_rows_sees_previous_writes() {
    let mut db = db_with(&["CREATE (:P {c:0})"]);
    run(
        &mut db,
        "MATCH (n:P) UNWIND [1,2,3] AS i SET n += {c: n.c + i}",
    );
    assert_eq!(int(&mut db, "MATCH (n:P) RETURN n.c"), 6);
}

// ── C6 (#17): standalone MERGE binds every match, with Cypher equality ────

#[test]
fn c6_standalone_merge_returns_all_matches() {
    let mut db = db_with(&["CREATE (:P {x:1, id:1}), (:P {x:1, id:2})"]);
    assert_eq!(
        ints(&mut db, "MERGE (n:P {x:1}) RETURN n.id"),
        vec![Some(1), Some(2)]
    );
    run(&mut db, "MERGE (n:P {x:1}) ON MATCH SET n.hit = true");
    assert_eq!(int(&mut db, "MATCH (n:P {hit: true}) RETURN count(n)"), 2);
    assert_eq!(node_count(&mut db, "P"), 2);
}

#[test]
fn c6_merge_float_matches_stored_integer() {
    let mut db = db_with(&["CREATE (:P {x:1})"]);
    assert_eq!(int(&mut db, "MERGE (n:P {x:1.0}) RETURN count(n)"), 1);
    assert_eq!(node_count(&mut db, "P"), 1);
    // A value that is not Cypher-equal (1.5) does not match: a new node.
    run(&mut db, "MERGE (n:P {x:1.5})");
    assert_eq!(node_count(&mut db, "P"), 2);
}

#[test]
fn c6_merge_float_matches_stored_integer_with_index() {
    let mut db = db_with(&["CREATE (:P {x:1})"]);
    db.begin_write().unwrap();
    db.create_index("P", "x").unwrap();
    db.commit().unwrap();
    assert_eq!(int(&mut db, "MERGE (n:P {x:1.0}) RETURN count(n)"), 1);
    assert_eq!(node_count(&mut db, "P"), 1);
}

// ── C7 (appendix C5): plain integers are not node references ──────────────

#[test]
fn c7_create_edge_between_integers_is_type_error() {
    let mut db = db_with(&["CREATE (:A), (:B)"]);
    assert_query_err(
        &mut db,
        "WITH 1 AS a, 2 AS b CREATE (a)-[:R]->(b)",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_query_err(
        &mut db,
        "WITH 50 AS a, 51 AS b CREATE (a)-[:Z]->(b)",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_eq!(edge_count(&mut db), 0);
}

#[test]
fn c7_merge_edge_between_integers_is_type_error() {
    let mut db = db_with(&["CREATE (:A), (:B)"]);
    assert_query_err(
        &mut db,
        "WITH 1 AS a, 2 AS b MERGE (a)-[:R]->(b)",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_eq!(edge_count(&mut db), 0);
}

#[test]
fn c7_map_merge_with_integer_is_type_error() {
    let mut db = db_with(&["CREATE (:A {secret: 'x'})"]);
    assert_query_err(
        &mut db,
        "MERGE (n:N2 {k:1}) ON CREATE SET n += 1",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_eq!(node_count(&mut db, "N2"), 0);
}

#[test]
fn c7_create_edge_from_node_value_uses_that_node() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    run(
        &mut db,
        "MATCH (a:A) WITH head(collect(a)) AS x CREATE (x)-[:R]->(:Q)",
    );
    assert_eq!(int(&mut db, "MATCH (a:A)-[:R]->(:Q) RETURN a.id"), 1);
    assert_eq!(node_count(&mut db, "A"), 1);
}

#[test]
fn c7_create_edge_from_deleted_node_errors() {
    let mut db = db_with(&["CREATE (:A)"]);
    let e = err(&mut db, "MATCH (a:A) DELETE a CREATE (a)-[:R]->(:B)");
    assert!(
        matches!(e, GraphError::NodeNotFound { .. }),
        "expected NodeNotFound, got {e:?}"
    );
    assert_eq!(edge_count(&mut db), 0);
    assert_eq!(node_count(&mut db, "A"), 1);
}

#[test]
fn c7_null_endpoint_errors_instead_of_dropping_the_row() {
    let mut db = db_with(&["CREATE (:B)"]);
    for q in [
        "OPTIONAL MATCH (a:Nope) MERGE (a)-[:R]->(b:B)",
        "OPTIONAL MATCH (a:Nope) CREATE (a)-[:R]->(:B)",
    ] {
        assert_query_err(&mut db, q, "SemanticError", ErrorCode::Other);
    }
    assert_eq!(edge_count(&mut db), 0);
    assert_eq!(node_count(&mut db, "B"), 1);
}

// ── C8 (appendix I2): standalone relationship MERGE is whole-pattern ──────

#[test]
fn c8_self_loop_merge_uses_one_node() {
    let mut db = db_with(&["CREATE (:A {id:9})"]);
    run(&mut db, "MERGE (a:A {id:1})-[:R]->(a)");
    assert_eq!(int(&mut db, "MATCH (a:A {id:1})-[:R]->(b) RETURN b.id"), 1);
    run(&mut db, "MERGE (a:A {id:1})-[:R]->(a)");
    assert_eq!(node_count(&mut db, "A"), 2);
    assert_eq!(edge_count(&mut db), 1);
}

#[test]
fn c8_partial_match_creates_whole_pattern() {
    let mut db = db_with(&["CREATE (:A {id:1})"]);
    run(&mut db, "MERGE (a:A {id:1})-[:R]->(b:B {id:2})");
    assert_eq!(node_count(&mut db, "A"), 2);
    assert_eq!(node_count(&mut db, "B"), 1);
    run(&mut db, "MERGE (a:A {id:1})-[:R]->(b:B {id:2})");
    assert_eq!(node_count(&mut db, "A"), 2);
    assert_eq!(edge_count(&mut db), 1);
}

// ── C9 (appendix I3): SET on a deleted relationship ───────────────────────

#[test]
fn c9_set_on_deleted_relationship_errors() {
    let mut db = db_with(&["CREATE (:X)-[:R]->(:Y)"]);
    let e = err(&mut db, "MATCH ()-[r:R]->() DELETE r SET r.x = 1");
    assert!(
        matches!(e, GraphError::EdgeNotFound { .. }),
        "expected EdgeNotFound, got {e:?}"
    );
    let e = err(&mut db, "MATCH ()-[r:R]->() DELETE r SET r += {x: 1}");
    assert!(
        matches!(e, GraphError::EdgeNotFound { .. }),
        "expected EdgeNotFound, got {e:?}"
    );
    // The failed statements left exactly the original edge.
    assert_eq!(edge_count(&mut db), 1);
    run(&mut db, "MATCH ()-[r:R]->() DELETE r");
    run(&mut db, "MATCH (x:X), (y:Y) CREATE (x)-[:R]->(y)");
    assert_eq!(edge_count(&mut db), 1);
}

// ── C10 (appendix I5): one map resolver for SET = / += ────────────────────

#[test]
fn c10_set_to_string_is_type_error_everywhere() {
    let mut db = db_with(&["CREATE (:N {k:1, keep:2})"]);
    assert_query_err(
        &mut db,
        "MATCH (n:N) SET n = 'x'",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_query_err(
        &mut db,
        "MERGE (n:N {k:1}) ON MATCH SET n = 'x'",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_query_err(
        &mut db,
        "MERGE (n:N {k:2}) ON CREATE SET n += 'x'",
        "TypeError",
        ErrorCode::InvalidArgumentType,
    );
    assert_eq!(int(&mut db, "MATCH (n:N) RETURN n.keep"), 2);
}

#[test]
fn c10_set_copies_properties_from_node_and_relationship() {
    let mut db = db_with(&["CREATE (:A {p:1, q:2})-[:R {w:3}]->(:B {old:0})"]);
    run(&mut db, "MATCH (a:A), (b:B) SET b = a");
    assert_eq!(
        single(&mut db, "MATCH (b:B) RETURN properties(b)"),
        map(&[("p", Value::I64(1)), ("q", Value::I64(2))])
    );
    run(&mut db, "MATCH (:A)-[r:R]->(b:B) SET b += r");
    assert_eq!(
        single(&mut db, "MATCH (b:B) RETURN properties(b)"),
        map(&[
            ("p", Value::I64(1)),
            ("q", Value::I64(2)),
            ("w", Value::I64(3))
        ])
    );
    run(&mut db, "MATCH (a:A)-[r:R]->(:B) SET r = a");
    assert_eq!(
        single(&mut db, "MATCH ()-[r:R]->() RETURN properties(r)"),
        map(&[("p", Value::I64(1)), ("q", Value::I64(2))])
    );
}

// ── C11 (appendix M1): REMOVE label keeps the returned node in sync ───────

#[test]
fn c11_remove_label_updates_returned_node() {
    let mut db = db_with(&["CREATE (:A:B {k:1})"]);
    let rows = run(&mut db, "MATCH (n:A) REMOVE n:A RETURN n, labels(n)");
    assert_eq!(rows.len(), 1);
    assert_node(
        rows[0].get("n").unwrap(),
        &["B"],
        props(&[("k", Value::I64(1))]),
    );
    assert_eq!(rows[0].get("labels(n)"), Some(&Value::List(vec![s("B")])));
}

// ── C13 (#10 nodes part): SET map / REMOVE / labels through value bindings ─

#[test]
fn c13_set_map_on_value_bound_node() {
    let mut db = db_with(&["CREATE (:Q {a:1})"]);
    run(
        &mut db,
        "MATCH (n:Q) WITH head(collect(n)) AS m SET m = {b:2}",
    );
    assert_eq!(
        single(&mut db, "MATCH (n:Q) RETURN properties(n)"),
        map(&[("b", Value::I64(2))])
    );
    run(
        &mut db,
        "MATCH (n:Q) WITH head(collect(n)) AS m SET m += {c:3}",
    );
    assert_eq!(
        single(&mut db, "MATCH (n:Q) RETURN properties(n)"),
        map(&[("b", Value::I64(2)), ("c", Value::I64(3))])
    );
    run(&mut db, "MATCH (n:Q) WITH head(collect(n)) AS m REMOVE m.b");
    assert_eq!(
        single(&mut db, "MATCH (n:Q) RETURN properties(n)"),
        map(&[("c", Value::I64(3))])
    );
}

#[test]
fn c13_labels_on_value_bound_node() {
    let mut db = db_with(&["CREATE (:Q {a:1})"]);
    run(&mut db, "MATCH (n:Q) WITH [n][0] AS m SET m:L");
    assert_eq!(
        single(&mut db, "MATCH (n {a:1}) RETURN labels(n)"),
        Value::List(vec![s("L"), s("Q")])
    );
    run(&mut db, "MATCH (n:Q) WITH [n][0] AS m REMOVE m:Q");
    assert_eq!(
        single(&mut db, "MATCH (n {a:1}) RETURN labels(n)"),
        Value::List(vec![s("L")])
    );
}

#[test]
fn c13_set_map_and_remove_on_value_bound_relationship() {
    let mut db = db_with(&["CREATE (:X)-[:R {w:1}]->(:Y)"]);
    run(
        &mut db,
        "MATCH ()-[r:R]->() WITH head(collect(r)) AS x SET x = {v:9}",
    );
    assert_eq!(
        single(&mut db, "MATCH ()-[r:R]->() RETURN properties(r)"),
        map(&[("v", Value::I64(9))])
    );
    run(
        &mut db,
        "MATCH ()-[r:R]->() WITH head(collect(r)) AS x REMOVE x.v",
    );
    assert_eq!(
        single(&mut db, "MATCH ()-[r:R]->() RETURN properties(r)"),
        Value::Map(Default::default())
    );
}
