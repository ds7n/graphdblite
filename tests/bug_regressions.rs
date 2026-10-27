//! Regression tests for bugs reported by @arupkamal's October 2026 review
//! (github.com/arupkamal/graphdblite). shortestPath cases live in
//! `e2e_query_tests.rs`.
//!
//! Each test is named after the issue it covers (numbers match the review
//! report) and fails on the pre-fix code.

use std::collections::HashMap;

use graphdblite::{Database, NodeId, Record, Value};

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

fn int(db: &mut Database, q: &str) -> i64 {
    let rows = run(db, q);
    assert_eq!(rows.len(), 1, "expected one row from {q}, got {rows:?}");
    let rec = &rows[0];
    let v = rec
        .values()
        .next()
        .cloned()
        .unwrap_or_else(|| panic!("no columns in {rec:?}"));
    match v {
        Value::I64(n) => n,
        other => panic!("expected integer from {q}, got {other:?}"),
    }
}

fn strings(rows: &[Record], col: &str) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match r.get(col) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected string in {col}, got {other:?}"),
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

// ── #1 label scans: case-sensitive, `_` is not a wildcard ──────────────────

#[test]
fn bug01_label_scan_is_case_sensitive() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:Person {name: 'A'}), (:Admin:Person {name: 'B'})",
    );
    assert_eq!(int(&mut db, "MATCH (n:person) RETURN count(n)"), 0);
    assert_eq!(int(&mut db, "MATCH (n:PERSON) RETURN count(n)"), 0);
    assert_eq!(int(&mut db, "MATCH (n:Person) RETURN count(n)"), 2);
    assert_eq!(int(&mut db, "MATCH (n:admin) RETURN count(n)"), 0);
    assert_eq!(int(&mut db, "MATCH (n:Admin) RETURN count(n)"), 1);
}

#[test]
fn bug01_label_scan_underscore_is_literal() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:AxB {v: 1}), (:Q:AyB {v: 2}), (:A_B {v: 3})",
    );
    assert_eq!(int(&mut db, "MATCH (n:A_B) RETURN count(n)"), 1);
    assert_eq!(int(&mut db, "MATCH (n:A_B) RETURN n.v"), 3);
}

#[test]
fn bug01_api_find_nodes_by_label_exact() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:Person), (:AxB)");
    let tx = db.read_tx().unwrap();
    assert!(tx.find_nodes_by_label("person").unwrap().is_empty());
    assert!(tx.find_nodes_by_label("A_B").unwrap().is_empty());
    assert_eq!(tx.find_nodes_by_label("Person").unwrap().len(), 1);
    tx.commit().unwrap();
}

#[test]
fn bug01_index_backfill_and_merge_respect_label_case() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:Person {name: 'A'})");
    create_index(&mut db, "person", "name");
    assert_eq!(
        int(&mut db, "MATCH (n:person {name: 'A'}) RETURN count(n)"),
        0
    );
    // MERGE after another clause goes through the per-row path.
    run(&mut db, "UNWIND [1] AS i MERGE (n:person {name: 'A'})");
    assert_eq!(int(&mut db, "MATCH (n) RETURN count(n)"), 2);
    assert_eq!(int(&mut db, "MATCH (n:person) RETURN count(n)"), 1);
    assert_eq!(int(&mut db, "MATCH (n:Person) RETURN count(n)"), 1);
}

// ── #2 index lookup must keep the extra labels ─────────────────────────────

#[test]
fn bug02_index_lookup_keeps_extra_labels() {
    let mut db = Database::open_memory().unwrap();
    create_index(&mut db, "A", "x");
    run(&mut db, "CREATE (:A {x: 1}), (:A:B {x: 1}), (:A:C {x: 1})");
    assert_eq!(int(&mut db, "MATCH (n:A:B {x: 1}) RETURN count(n)"), 1);
    assert_eq!(int(&mut db, "MATCH (n:A:B:C {x: 1}) RETURN count(n)"), 0);
    assert_eq!(int(&mut db, "MATCH (n:A {x: 1}) RETURN count(n)"), 3);
}

// ── #3 integer / float equality through an index ──────────────────────────

#[test]
fn bug03_index_lookup_numeric_equality_matches_scan() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:P {x: 1.0}), (:P {x: 2}), (:P {x: 3.5})");
    let queries = [
        ("MATCH (n:P {x: 1}) RETURN count(n)", 1),
        ("MATCH (n:P {x: 2.0}) RETURN count(n)", 1),
        ("MATCH (n:P) WHERE n.x = 1 RETURN count(n)", 1),
        ("MATCH (n:P) WHERE n.x = 2.0 RETURN count(n)", 1),
        ("MATCH (n:P {x: 3.5}) RETURN count(n)", 1),
        ("MATCH (n:P {x: 3}) RETURN count(n)", 0),
    ];
    let without: Vec<i64> = queries.iter().map(|(q, _)| int(&mut db, q)).collect();
    create_index(&mut db, "P", "x");
    for ((q, expected), before) in queries.iter().zip(without) {
        assert_eq!(before, *expected, "scan result for {q}");
        assert_eq!(int(&mut db, q), *expected, "index result for {q}");
    }
    // Parameters go through the same path.
    let rows = run_p(
        &mut db,
        "MATCH (n:P {x: $v}) RETURN count(n) AS c",
        &[("v", Value::I64(1))],
    );
    assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
}

// ── #6 MERGE direction ────────────────────────────────────────────────────

#[test]
fn bug06_match_merge_incoming_direction() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:A), (:B)");
    run(&mut db, "MATCH (a:A), (b:B) MERGE (a)<-[:R]-(b)");
    assert_eq!(int(&mut db, "MATCH (:B)-[r:R]->(:A) RETURN count(r)"), 1);
    assert_eq!(int(&mut db, "MATCH (:A)-[r:R]->(:B) RETURN count(r)"), 0);
    // Re-running matches the existing b->a edge instead of creating one.
    run(&mut db, "MATCH (a:A), (b:B) MERGE (a)<-[:R]-(b)");
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN count(r)"), 1);
}

#[test]
fn bug06_standalone_merge_direction_and_undirected() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "MERGE (a:A {id: 1})<-[:R]-(b:B {id: 2})");
    assert_eq!(
        int(
            &mut db,
            "MATCH (:B {id: 2})-[r:R]->(:A {id: 1}) RETURN count(r)"
        ),
        1
    );
    assert_eq!(
        int(
            &mut db,
            "MATCH (:A {id: 1})-[r:R]->(:B {id: 2}) RETURN count(r)"
        ),
        0
    );
    // Undirected MERGE must match the existing edge in either direction.
    run(&mut db, "MERGE (a:A {id: 1})-[:R]-(b:B {id: 2})");
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN count(r)"), 1);
    run(
        &mut db,
        "MATCH (a:A {id: 1}), (b:B {id: 2}) MERGE (a)-[:R]-(b)",
    );
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN count(r)"), 1);
}

// ── #7 MERGE after another clause keeps every label ──────────────────────

#[test]
fn bug07_match_merge_keeps_all_labels() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:A {id: 1})");
    let rows = run(
        &mut db,
        "UNWIND [1] AS i MERGE (n:A:B {id: i}) RETURN labels(n) AS l",
    );
    assert_eq!(
        rows[0].get("l"),
        Some(&Value::List(vec![
            Value::String("A".into()),
            Value::String("B".into())
        ]))
    );
    assert_eq!(int(&mut db, "MATCH (n:A {id: 1}) RETURN count(n)"), 2);
    // Running again matches the A:B node only.
    run(&mut db, "UNWIND [1] AS i MERGE (n:A:B {id: i})");
    assert_eq!(int(&mut db, "MATCH (n:A {id: 1}) RETURN count(n)"), 2);
}

#[test]
fn bug07_relationship_merge_endpoints_keep_all_labels() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:A {id: 1})");
    run(&mut db, "MERGE (a:A:X {id: 1})-[:R]->(b:B:Y {id: 2})");
    assert_eq!(int(&mut db, "MATCH (n:A:X {id: 1}) RETURN count(n)"), 1);
    assert_eq!(int(&mut db, "MATCH (n:B:Y {id: 2}) RETURN count(n)"), 1);
    assert_eq!(
        int(&mut db, "MATCH (:A:X)-[r:R]->(:B:Y) RETURN count(r)"),
        1
    );
}

// ── #8 ON CREATE SET targets the newly created parallel edge ─────────────

#[test]
fn bug08_merge_on_create_targets_new_parallel_edge() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (a:A)-[:T {name: 'r1'}]->(b:B)");
    let rows = run(
        &mut db,
        "MATCH (a:A), (b:B) MERGE (a)-[r:T {name: 'r2'}]->(b) \
         ON CREATE SET r.created = true RETURN r.name AS name, r.created AS created",
    );
    assert_eq!(rows[0].get("name"), Some(&Value::String("r2".into())));
    assert_eq!(rows[0].get("created"), Some(&Value::Bool(true)));
    let rows = run(
        &mut db,
        "MATCH ()-[r:T]->() RETURN r.name AS name, r.created AS created ORDER BY name",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("created"), Some(&Value::Null));
    assert_eq!(rows[1].get("created"), Some(&Value::Bool(true)));
}

#[test]
fn bug08_standalone_merge_on_create_targets_new_parallel_edge() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:A {id: 1})-[:T {name: 'r1'}]->(:B {id: 2})",
    );
    run(
        &mut db,
        "MERGE (a:A {id: 1})-[r:T {name: 'r2'}]->(b:B {id: 2}) ON CREATE SET r.created = true",
    );
    let rows = run(
        &mut db,
        "MATCH ()-[r:T]->() RETURN r.name AS name, r.created AS created ORDER BY name",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("created"), Some(&Value::Null));
    assert_eq!(rows[1].get("created"), Some(&Value::Bool(true)));
}

// ── #9 var-length relationship property filters with non-literals ────────

#[test]
fn bug09_var_length_property_filter_with_parameter_and_variable() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (a:N {name: 'a', w: 1})-[:R {w: 1}]->(b:N {name: 'b'})-[:R {w: 2}]->(c:N {name: 'c'})",
    );
    let rows = run_p(
        &mut db,
        "MATCH (a:N {name: 'a'})-[:R*1..2 {w: $w}]->(x) RETURN x.name AS name",
        &[("w", Value::I64(1))],
    );
    assert_eq!(strings(&rows, "name"), vec!["b"]);
    let rows = run(
        &mut db,
        "MATCH (a:N {name: 'a'}) WITH a, a.w AS w MATCH (a)-[:R*1..2 {w: w}]->(x) RETURN x.name AS name",
    );
    assert_eq!(strings(&rows, "name"), vec!["b"]);
    let rows = run(
        &mut db,
        "MATCH (a:N {name: 'a'})-[:R*1..2 {w: 1 + 1}]->(x) RETURN x.name AS name",
    );
    assert!(rows.is_empty(), "first hop has w=1, so no path: {rows:?}");
}

// ── #10 DELETE / SET on a non-entity value ───────────────────────────────

#[test]
fn bug10_delete_integer_variable_is_an_error() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:N {k: 1})");
    // The node's id equals its `k` property, so a bare integer binding would
    // previously be treated as a node reference.
    assert_eq!(int(&mut db, "MATCH (n:N) RETURN id(n)"), 1);
    assert!(
        db.execute("MATCH (n:N) WITH n.k AS x DELETE x").is_err(),
        "deleting an integer must fail"
    );
    assert_eq!(int(&mut db, "MATCH (n:N) RETURN count(n)"), 1);
    assert!(
        db.execute("MATCH (n:N) WITH n.k AS x SET x.k = 2").is_err(),
        "SET on an integer must fail"
    );
    assert!(
        db.execute("MATCH (n:N) WITH n.k AS x SET x:Foo").is_err(),
        "SET label on an integer must fail"
    );
    assert!(
        db.execute("MATCH (n:N) WITH n.k AS x REMOVE x.k").is_err(),
        "REMOVE on an integer must fail"
    );
    assert_eq!(int(&mut db, "MATCH (n:N) RETURN n.k"), 1);
    assert_eq!(int(&mut db, "MATCH (n:Foo) RETURN count(n)"), 0);
    // Null bindings (e.g. from OPTIONAL MATCH) are still silently skipped.
    run(&mut db, "OPTIONAL MATCH (m:Missing) DELETE m");
    run(&mut db, "OPTIONAL MATCH (m:Missing) SET m.k = 1");
    // Normal entity deletes still work, including through WITH.
    run(&mut db, "MATCH (n:N) WITH n DELETE n");
    assert_eq!(int(&mut db, "MATCH (n) RETURN count(n)"), 0);
}

// ── #11 case-insensitive full-text index rewrites ────────────────────────

fn fts_ci_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index_ci("Person", "name").unwrap();
    db.commit().unwrap();
    run(
        &mut db,
        "CREATE (:Person {name: 'Alice Smith'}), (:Person {name: 'bob jones'})",
    );
    db
}

#[test]
fn bug11_tolower_idiom_with_ci_index_matches_scan_semantics() {
    let mut db = fts_ci_db();
    for (q, n) in [
        (
            "MATCH (n:Person) WHERE toLower(n.name) STARTS WITH toLower('ALICE') RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE toLower(n.name) ENDS WITH toLower('SMITH') RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('CE SM') RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('AL') RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE toLower(n.name) STARTS WITH toLower('BO') RETURN count(n)",
            1,
        ),
    ] {
        assert_eq!(int(&mut db, q), n, "{q}");
    }
}

#[test]
fn bug11_plain_predicates_stay_case_sensitive_with_ci_index() {
    // An index never changes results: plain predicates keep Cypher's
    // case-sensitive semantics even when a CI index serves them, for indexed
    // terms and for short terms that fall back to a scan.
    let mut db = fts_ci_db();
    for (q, n) in [
        (
            "MATCH (n:Person) WHERE n.name CONTAINS 'alice' RETURN count(n)",
            0,
        ),
        (
            "MATCH (n:Person) WHERE n.name CONTAINS 'Alice' RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE n.name STARTS WITH 'alice' RETURN count(n)",
            0,
        ),
        (
            "MATCH (n:Person) WHERE n.name ENDS WITH 'SMITH' RETURN count(n)",
            0,
        ),
        (
            "MATCH (n:Person) WHERE n.name ENDS WITH 'Smith' RETURN count(n)",
            1,
        ),
        (
            "MATCH (n:Person) WHERE n.name CONTAINS 'al' RETURN count(n)",
            0,
        ),
        (
            "MATCH (n:Person) WHERE n.name CONTAINS 'Al' RETURN count(n)",
            1,
        ),
    ] {
        assert_eq!(int(&mut db, q), n, "{q}");
    }
}

// ── #12 cached plans after rollback / DDL from another handle ─────────────

#[test]
fn bug12_plan_cache_survives_rolled_back_index() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:P {x: 1})");
    let q = "MATCH (n:P {x: 1}) RETURN count(n) AS c";
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("P", "x").unwrap();
        assert_eq!(tx.query(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
        tx.rollback().unwrap();
    }
    {
        let tx = db.read_tx().unwrap();
        let rows = tx.query(q).expect("stale cached plan after rollback");
        assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
        tx.commit().unwrap();
    }
    // Same through the stateful API and a dropped guard.
    db.begin_write().unwrap();
    db.create_index("P", "x").unwrap();
    assert_eq!(db.execute(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
    db.rollback().unwrap();
    assert_eq!(db.execute(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
    {
        let tx = db.write_tx().unwrap();
        tx.create_index("P", "x").unwrap();
        tx.query(q).unwrap();
        // dropped without commit -> rollback
    }
    assert_eq!(db.execute(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
}

#[test]
fn bug12_plan_cache_sees_index_dropped_by_other_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.db");
    let mut a = Database::open(&path).unwrap();
    let mut b = Database::open(&path).unwrap();
    run(&mut a, "CREATE (:P {x: 1})");
    create_index(&mut a, "P", "x");
    let q = "MATCH (n:P {x: 1}) RETURN count(n) AS c";
    assert_eq!(a.execute(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
    b.begin_write().unwrap();
    b.drop_index("P", "x").unwrap();
    b.commit().unwrap();
    let rows = a
        .execute(q)
        .expect("stale cached plan after foreign DROP INDEX");
    assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
    // Typed transactions use the same cache.
    create_index(&mut b, "P", "x");
    let tx = a.read_tx().unwrap();
    assert_eq!(tx.query(q).unwrap()[0].get("c"), Some(&Value::I64(1)));
    tx.commit().unwrap();
    b.begin_write().unwrap();
    b.drop_index("P", "x").unwrap();
    b.commit().unwrap();
    let tx = a.read_tx().unwrap();
    let rows = tx.query(q).expect("stale cached plan in read_tx");
    assert_eq!(rows[0].get("c"), Some(&Value::I64(1)));
    tx.commit().unwrap();
}

// ── #13 setting a property to null must not leave a Null index entry ─────

#[test]
fn bug13_api_set_null_does_not_index_null() {
    let mut db = Database::open_memory().unwrap();
    create_index(&mut db, "P", "email");
    let id = {
        let tx = db.write_tx().unwrap();
        let mut props = HashMap::new();
        props.insert("email".to_string(), Value::String("a@x".into()));
        let id = tx.create_node("P", props).unwrap();
        tx.set_node_property(id, "email", Value::Null).unwrap();
        tx.commit().unwrap();
        id
    };
    assert_eq!(int(&mut db, "MATCH (n:P {email: null}) RETURN count(n)"), 0);
    assert_eq!(
        int(&mut db, "MATCH (n:P) WHERE n.email = null RETURN count(n)"),
        0
    );
    {
        let tx = db.write_tx().unwrap();
        tx.delete_node(id).unwrap();
        tx.commit().unwrap();
    }
    // Before the fix the dangling entry made this lookup fail with NodeNotFound.
    assert_eq!(int(&mut db, "MATCH (n:P {email: null}) RETURN count(n)"), 0);
    let _ = NodeId(0);
}

#[test]
fn bug13_merge_set_null_does_not_index_null() {
    let mut db = Database::open_memory().unwrap();
    create_index(&mut db, "P", "email");
    run(&mut db, "CREATE (:P {id: 1, email: 'a@x'})");
    run(&mut db, "MERGE (n:P {id: 1}) ON MATCH SET n.email = null");
    let rows = run(&mut db, "MATCH (n:P {id: 1}) RETURN n.email AS e");
    assert_eq!(rows[0].get("e"), Some(&Value::Null));
    assert_eq!(int(&mut db, "MATCH (n:P {email: null}) RETURN count(n)"), 0);
    run(&mut db, "MATCH (n:P) DELETE n");
    assert_eq!(int(&mut db, "MATCH (n:P {email: null}) RETURN count(n)"), 0);
}

// ── #14 long non-ASCII names must not panic ──────────────────────────────

#[test]
fn bug14_long_multibyte_name_returns_error_not_panic() {
    let mut db = Database::open_memory().unwrap();
    let long_key = "日".repeat(86); // 258 bytes > 256 default limit
    let tx = db.write_tx().unwrap();
    let mut props = HashMap::new();
    props.insert(long_key.clone(), Value::I64(1));
    assert!(tx.create_node("L", props).is_err());
    assert!(tx.create_node(&long_key, HashMap::new()).is_err());
    tx.rollback().unwrap();
}

// ── #15 Cypher writes validate property types and size limits ────────────

#[test]
fn bug15_cypher_writes_reject_map_properties() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:N {id: 1})-[:R]->(:N {id: 2})");
    for q in [
        "CREATE (n {m: {a: 1}})",
        "CREATE (n {m: [{a: 1}]})",
        "MATCH (a:N {id: 1}), (b:N {id: 2}) CREATE (a)-[:S {m: {a: 1}}]->(b)",
        "MERGE (n:X {m: {a: 1}})",
        "MERGE (n:N {id: 1}) ON MATCH SET n.m = {a: 1}",
        "MERGE (n:Y {id: 1}) ON CREATE SET n.m = {a: 1}",
        "MATCH (n:N {id: 1}) SET n = {m: {a: 1}}",
        "MATCH (n:N {id: 1}) SET n += {m: {a: 1}}",
        "MATCH (a:N {id: 1}), (b:N {id: 2}) MERGE (a)-[r:R]->(b) ON MATCH SET r.m = {a: 1}",
    ] {
        assert!(db.execute(q).is_err(), "expected error for {q}");
    }
    assert_eq!(int(&mut db, "MATCH (n) RETURN count(n)"), 2);
    assert_eq!(
        int(&mut db, "MATCH (n) WHERE n.m IS NOT NULL RETURN count(n)"),
        0
    );
    assert_eq!(
        int(
            &mut db,
            "MATCH ()-[r]->() WHERE r.m IS NOT NULL RETURN count(r)"
        ),
        0
    );
}

#[test]
fn bug15_cypher_writes_enforce_size_limits() {
    let mut db = Database::open_memory().unwrap();
    let big = "x".repeat(1024 * 1024 + 1);
    let r = db.execute_with_params(
        "CREATE (n:N {s: $s})",
        Some(&HashMap::from([(
            "s".to_string(),
            Value::String(big.clone()),
        )])),
    );
    assert!(r.is_err(), "oversized property must be rejected");
    run(&mut db, "CREATE (n:N {id: 1})");
    let r = db.execute_with_params(
        "MATCH (n:N) SET n.s = $s",
        Some(&HashMap::from([("s".to_string(), Value::String(big))])),
    );
    assert!(r.is_err(), "oversized SET must be rejected");
    let long_label = "L".repeat(300);
    assert!(db.execute(&format!("CREATE (:{long_label})")).is_err());
    // Values under the limit are fine.
    run_p(
        &mut db,
        "MATCH (n:N) SET n.s = $s",
        &[("s", Value::String("x".repeat(1024)))],
    );
}

// ── Additional edge cases / non-regression checks for the fixes above ─────

#[test]
fn extra_index_equality_lists_and_composites() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (:P {l: [1.0, 2], a: 1, b: 2.0}), (:P {l: [1, 3], a: 1.0, b: 3})",
    );
    db.begin_write().unwrap();
    db.create_index("P", "l").unwrap();
    db.create_composite_index("P", &["a", "b"]).unwrap();
    db.commit().unwrap();
    assert_eq!(int(&mut db, "MATCH (n:P {l: [1, 2]}) RETURN count(n)"), 1);
    assert_eq!(
        int(&mut db, "MATCH (n:P {l: [1.0, 3.0]}) RETURN count(n)"),
        1
    );
    assert_eq!(int(&mut db, "MATCH (n:P {a: 1, b: 2}) RETURN count(n)"), 1);
    assert_eq!(int(&mut db, "MATCH (n:P {a: 1}) RETURN count(n)"), 2);
    assert_eq!(
        int(&mut db, "MATCH (n:P {a: 1.0, b: 3.0}) RETURN count(n)"),
        1
    );
    // Lists containing null never compare equal.
    assert_eq!(
        int(&mut db, "MATCH (n:P {l: [1, null]}) RETURN count(n)"),
        0
    );
    // A long numeric list exceeds the expansion cap and falls back to a scan.
    run(&mut db, "CREATE (:P {l: [1, 2, 3, 4, 5, 6, 7, 8]})");
    assert_eq!(
        int(
            &mut db,
            "MATCH (n:P {l: [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]}) RETURN count(n)"
        ),
        1
    );
}

#[test]
fn extra_var_length_filter_property_reference() {
    let mut db = Database::open_memory().unwrap();
    run(
        &mut db,
        "CREATE (a:N {name: 'a', w: 1})-[:R {w: 1}]->(:N {name: 'b'})-[:R {w: 2}]->(:N {name: 'c'})",
    );
    let rows = run(
        &mut db,
        "MATCH (a:N {name: 'a'})-[:R*1..2 {w: a.w}]->(x) RETURN x.name AS name",
    );
    assert_eq!(strings(&rows, "name"), vec!["b"]);
    // Literal filters keep working.
    let rows = run(
        &mut db,
        "MATCH (a:N {name: 'a'})-[:R*1..2 {w: 1}]->(x) RETURN x.name AS name",
    );
    assert_eq!(strings(&rows, "name"), vec!["b"]);
}

#[test]
fn extra_entity_writes_through_with_and_lists_still_work() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:N {k: 1}), (:N {k: 2}), (:N {k: 3})");
    run(&mut db, "MATCH (n:N {k: 1}) WITH n AS m SET m.k = 10");
    assert_eq!(int(&mut db, "MATCH (n:N {k: 10}) RETURN count(n)"), 1);
    run(&mut db, "MATCH (n:N {k: 10}) WITH n AS m REMOVE m.k");
    assert_eq!(
        int(&mut db, "MATCH (n:N) WHERE n.k IS NULL RETURN count(n)"),
        1
    );
    run(
        &mut db,
        "MATCH (n:N) WHERE n.k IS NULL WITH n AS m DELETE m",
    );
    assert_eq!(int(&mut db, "MATCH (n:N) RETURN count(n)"), 2);
    run(
        &mut db,
        "MATCH (n:N) WITH collect(n) AS ns UNWIND ns AS x SET x.seen = true",
    );
    assert_eq!(int(&mut db, "MATCH (n:N {seen: true}) RETURN count(n)"), 2);
    run(
        &mut db,
        "MATCH (n:N) WITH collect(n) AS ns UNWIND ns AS x DELETE x",
    );
    assert_eq!(int(&mut db, "MATCH (n) RETURN count(n)"), 0);
    // Relationship deletes via a list and via a path.
    run(
        &mut db,
        "CREATE (a:A)-[:R]->(b:B), (a)-[:R]->(:B), (c:C)-[:S]->(d:D)",
    );
    run(
        &mut db,
        "MATCH ()-[r:R]->() WITH collect(r) AS rs DELETE rs[0]",
    );
    assert_eq!(int(&mut db, "MATCH ()-[r:R]->() RETURN count(r)"), 1);
    run(&mut db, "MATCH p = (:C)-[:S]->(:D) DELETE p");
    assert_eq!(int(&mut db, "MATCH (n:C) RETURN count(n)"), 0);
    // Deleting a string is a type error too.
    assert!(db.execute("MATCH (a:A) WITH 'x' AS s DELETE s").is_err());
}

#[test]
fn extra_merge_incoming_path_variable_orientation() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:A {name: 'a'}), (:B {name: 'b'})");
    let rows = run(
        &mut db,
        "MATCH (a:A), (b:B) MERGE p = (a)<-[r:R]-(b) \
         RETURN [n IN nodes(p) | n.name] AS names, startNode(r).name AS s, endNode(r).name AS e",
    );
    assert_eq!(
        rows[0].get("names"),
        Some(&Value::List(vec![
            Value::String("a".into()),
            Value::String("b".into())
        ]))
    );
    assert_eq!(rows[0].get("s"), Some(&Value::String("b".into())));
    assert_eq!(rows[0].get("e"), Some(&Value::String("a".into())));
}

#[test]
fn extra_parallel_edge_set_after_create_targets_new_edge() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (a:A)-[:T {n: 1}]->(b:B)");
    run(
        &mut db,
        "MATCH (a:A), (b:B) CREATE (a)-[r:T {n: 2}]->(b) SET r.tag = 'new'",
    );
    let rows = run(
        &mut db,
        "MATCH ()-[r:T]->() RETURN r.n AS n, r.tag AS tag ORDER BY n",
    );
    assert_eq!(rows[0].get("tag"), Some(&Value::Null));
    assert_eq!(rows[1].get("tag"), Some(&Value::String("new".into())));
}

// ── #17 MERGE ON CREATE / ON MATCH can read the merged node ───────────────

#[test]
fn bug17_merge_set_can_read_merged_node_properties() {
    let mut db = Database::open_memory().unwrap();
    run(&mut db, "CREATE (:P {name: 'A', age: 1})");
    let rows = run(
        &mut db,
        "MERGE (p:P {name: 'A'}) ON MATCH SET p.visits = coalesce(p.visits, 0) + 1 \
         RETURN p.visits AS v",
    );
    assert_eq!(rows[0].get("v"), Some(&Value::I64(1)));
    let rows = run(
        &mut db,
        "MERGE (p:P {name: 'A'}) ON MATCH SET p.visits = coalesce(p.visits, 0) + 1 \
         RETURN p.visits AS v",
    );
    assert_eq!(rows[0].get("v"), Some(&Value::I64(2)));
    run(
        &mut db,
        "MERGE (p:P {name: 'A'}) ON MATCH SET p += {doubled: p.age * 2}",
    );
    assert_eq!(int(&mut db, "MATCH (p:P) RETURN p.doubled"), 2);
    let rows = run(
        &mut db,
        "UNWIND [1] AS i MERGE (p:P {name: 'B'}) ON CREATE SET p.label = p.name + '!' \
         RETURN p.label AS l",
    );
    assert_eq!(rows[0].get("l"), Some(&Value::String("B!".into())));
}
