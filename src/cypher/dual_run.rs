//! Dual-run test harness for record-v2 migration.
//!
//! Runs a Cypher plan through both `execute_with_ctx_named` (legacy
//! `IndexMap` path) and `execute_with_ctx_slot` (slot-indexed path) and
//! asserts the two paths produce equivalent results. Phase 3a stub:
//! `_slot` currently delegates to `_named`, so every comparison trivially
//! agrees. As Phase 3b–3g migrate operators, this harness becomes the
//! primary regression gate — any divergence between the two impls fails
//! its assertion before TCK ever sees the plan.
//!
//! See `plans/record-v2.md` Phase 3a.

#![cfg(test)]

use std::collections::HashMap;

use crate::cypher::executor::{execute_with_ctx_named, execute_with_ctx_slot, ExecContext};
use crate::cypher::record::NamedRecord;
use crate::types::Value;
use crate::Database;

/// Plan `cypher` and execute it twice — once via the named path, once via
/// the slot path. Returns the named-path result (the reference) on
/// agreement; panics with a diff on mismatch.
///
/// The comparison is multiset-equality by default (Cypher results are
/// unordered unless `ORDER BY` is present). When `ordered` is true,
/// records are compared positionally — use this for queries with
/// `ORDER BY`, `LIMIT N`, or `SKIP N`.
fn dual_run(db: &Database, cypher: &str, ordered: bool) -> Vec<NamedRecord> {
    let conn = db.connection();
    let stmt = crate::cypher::parser::parse(cypher).expect("parse");
    let plan = crate::cypher::planner::plan(conn, &stmt).expect("plan");
    let ctx = ExecContext::default();

    let v1 = execute_with_ctx_named(conn, &plan, &ctx).expect("named exec");
    let v2 = execute_with_ctx_slot(conn, &plan, &ctx).expect("slot exec");

    if ordered {
        assert_records_eq_ordered(&v1, &v2, cypher);
    } else {
        assert_records_eq_multiset(&v1, &v2, cypher);
    }

    v1
}

fn assert_records_eq_ordered(v1: &[NamedRecord], v2: &[NamedRecord], cypher: &str) {
    assert_eq!(
        v1.len(),
        v2.len(),
        "row count differs for `{cypher}`: named={} slot={}",
        v1.len(),
        v2.len()
    );
    for (i, (a, b)) in v1.iter().zip(v2.iter()).enumerate() {
        assert_eq!(
            a, b,
            "row {i} differs for `{cypher}`:\n  named={a:?}\n   slot={b:?}"
        );
    }
}

fn assert_records_eq_multiset(v1: &[NamedRecord], v2: &[NamedRecord], cypher: &str) {
    assert_eq!(
        v1.len(),
        v2.len(),
        "row count differs for `{cypher}`: named={} slot={}",
        v1.len(),
        v2.len()
    );
    let mut counts: HashMap<Vec<(String, Value)>, isize> = HashMap::new();
    for r in v1 {
        *counts
            .entry(
                r.fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
            .or_default() += 1;
    }
    for r in v2 {
        *counts
            .entry(
                r.fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
            .or_default() -= 1;
    }
    let leftover: Vec<_> = counts.into_iter().filter(|(_, c)| *c != 0).collect();
    assert!(
        leftover.is_empty(),
        "multiset diff for `{cypher}`: {leftover:?}"
    );
}

// ---------------------------------------------------------------------------
// Smoke tests — read-side queries the slot path will own as Phase 3
// progresses. Phase 3a: all are trivial because `_slot` delegates to
// `_named`. Phase 3b+ replaces operators one at a time and these
// assertions become real regression gates.

fn fresh_db() -> Database {
    let mut db = Database::open_memory().unwrap();
    db.execute(
        "CREATE (a:Person {name: 'Alice', age: 30}),
                (b:Person {name: 'Bob', age: 25}),
                (c:Person {name: 'Carol', age: 40}),
                (a)-[:KNOWS {since: 2020}]->(b),
                (b)-[:KNOWS {since: 2021}]->(c),
                (a)-[:KNOWS {since: 2022}]->(c)",
    )
    .unwrap();
    db
}

#[test]
fn dual_run_match_return_property() {
    let db = fresh_db();
    let rows = dual_run(&db, "MATCH (p:Person) RETURN p.name", false);
    assert_eq!(rows.len(), 3);
}

#[test]
fn dual_run_match_where_return() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (p:Person) WHERE p.age > 25 RETURN p.name AS nm",
        false,
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn dual_run_match_expand() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.name, b.name",
        false,
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn dual_run_aggregate() {
    let db = fresh_db();
    let rows = dual_run(&db, "MATCH (p:Person) RETURN count(*)", false);
    assert_eq!(rows.len(), 1);
}

#[test]
fn dual_run_order_by_limit() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (p:Person) RETURN p.name AS nm ORDER BY p.age DESC LIMIT 2",
        true,
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn dual_run_var_length_path() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (a:Person {name: 'Alice'})-[:KNOWS*1..2]->(c:Person) RETURN c.name AS nm",
        false,
    );
    assert!(rows.len() >= 2);
}

#[test]
fn dual_run_unwind() {
    let db = fresh_db();
    let rows = dual_run(&db, "UNWIND [1, 2, 3] AS x RETURN x", true);
    assert_eq!(rows.len(), 3);
}

#[test]
fn dual_run_with_chain() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (p:Person) WITH p.age AS yrs WHERE yrs > 25 RETURN yrs ORDER BY yrs",
        true,
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn dual_run_typed_expand_returns_rel_property() {
    // Note: unlabeled `[r]` patterns fall back to the named path because
    // `iter::ExpandIter` (which the slot path wraps) has known gaps for
    // type discovery; see is_slot_supported in iter_slot.rs.
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE ()-[:T {num: 1}]->()").unwrap();
    let rows = dual_run(&db, "MATCH ()-[r:T]->() RETURN r.num", false);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("r.num"), Some(&Value::I64(1)));
}

#[test]
fn dual_run_optional_match() {
    let db = fresh_db();
    let rows = dual_run(
        &db,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[r:KNOWS]->(q) RETURN p.name, q.name",
        false,
    );
    assert!(!rows.is_empty());
}
