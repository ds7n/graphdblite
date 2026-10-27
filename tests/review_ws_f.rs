//! Regression tests for the WS-F review findings (bindings + transactions):
//! statement-level atomicity inside explicit transactions (D3) and
//! `:memory:` opening an in-memory database (F12).
//!
//! Each test fails on the pre-fix code.

use graphdblite::{Database, GraphError, QueryError, Value};

/// A statement that writes a `:M` node for its first row, then fails with a
/// runtime type error on the second row (`'a' - 1`).
const FAILS_AFTER_WRITING: &str = "UNWIND [1, 'a'] AS i CREATE (:M {v: 1}) RETURN i - 1 AS x";

fn count(db: &mut Database, label: &str) -> i64 {
    let rows = db
        .execute(&format!("MATCH (n:{label}) RETURN count(n) AS c"))
        .unwrap();
    match rows[0].get("c") {
        Some(Value::I64(n)) => *n,
        other => panic!("expected an integer count, got {other:?}"),
    }
}

fn assert_runtime_type_error(err: &GraphError) {
    match err {
        GraphError::Query(q @ QueryError::TypeError { .. }) => {
            assert_eq!(q.kind(), "TypeError", "unexpected error: {err}")
        }
        other => panic!("expected a Cypher TypeError, got {other}"),
    }
}

// D3: a failed statement inside an explicit (stateful) write transaction is
// rolled back to its own savepoint. The transaction stays usable and the
// statements around it commit; the failed statement's writes do not.
#[test]
fn stateful_tx_failed_statement_is_rolled_back_alone() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (:K)").unwrap();
    let err = db.execute(FAILS_AFTER_WRITING).unwrap_err();
    assert_runtime_type_error(&err);
    db.execute("CREATE (:K2)").unwrap();
    db.commit().unwrap();

    assert_eq!(count(&mut db, "M"), 0);
    assert_eq!(count(&mut db, "K"), 1);
    assert_eq!(count(&mut db, "K2"), 1);
}

// D3 for the typed `WriteTransaction::query` path.
#[test]
fn typed_write_tx_failed_statement_is_rolled_back_alone() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE (:K)").unwrap();
        let err = tx.query(FAILS_AFTER_WRITING).unwrap_err();
        assert_runtime_type_error(&err);
        tx.query("CREATE (:K2)").unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(count(&mut db, "M"), 0);
    assert_eq!(count(&mut db, "K"), 1);
    assert_eq!(count(&mut db, "K2"), 1);
}

// D3: the per-statement savepoint is released on success, so an explicit
// rollback still discards every statement of the transaction (BC-01).
#[test]
fn explicit_rollback_still_discards_all_statements() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (:K)").unwrap();
    db.execute(FAILS_AFTER_WRITING).unwrap_err();
    db.execute("CREATE (:K2)").unwrap();
    db.rollback().unwrap();

    assert_eq!(count(&mut db, "K"), 0);
    assert_eq!(count(&mut db, "K2"), 0);
    assert_eq!(count(&mut db, "M"), 0);
}

// D3: several failed statements in a row each roll back cleanly; the
// savepoint is not left behind (a leftover savepoint would make the final
// COMMIT keep it open or fail).
#[test]
fn repeated_failures_keep_the_transaction_usable() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    for _ in 0..3 {
        db.execute(FAILS_AFTER_WRITING).unwrap_err();
    }
    db.execute("CREATE (:K)").unwrap();
    db.commit().unwrap();
    assert_eq!(count(&mut db, "M"), 0);
    assert_eq!(count(&mut db, "K"), 1);
    // No transaction is left open after the commit.
    db.begin_write().unwrap();
    db.rollback().unwrap();
}

// D3: a failed DDL-free write inside a write tx does not undo the earlier
// index DDL of the same transaction.
#[test]
fn failed_statement_keeps_earlier_ddl() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_index("P", "k").unwrap();
    db.execute(FAILS_AFTER_WRITING).unwrap_err();
    db.commit().unwrap();
    let rows = db
        .execute("CALL db.indexes() YIELD label, property RETURN label, property")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("label"), Some(&Value::String("P".into())));
    assert_eq!(rows[0].get("property"), Some(&Value::String("k".into())));
}

// F12: `:memory:` opens an in-memory database instead of creating a file
// literally named `:memory:` in the working directory.
#[test]
fn open_memory_path_is_in_memory() {
    let marker = std::path::Path::new(":memory:");
    assert!(!marker.exists(), "stale ':memory:' file in the working dir");
    let mut a = Database::open(":memory:").unwrap();
    a.execute("CREATE (:X)").unwrap();
    assert!(
        !marker.exists(),
        "opening ':memory:' created a file in the working directory"
    );
    // Each in-memory database is private to its handle.
    let mut b = Database::open(":memory:").unwrap();
    assert_eq!(count(&mut a, "X"), 1);
    assert_eq!(count(&mut b, "X"), 0);
}

// F12: the configured open path honors `:memory:` too.
#[test]
fn open_with_config_memory_path_is_in_memory() {
    let mut db = Database::open_with_config(":memory:", graphdblite::Config::default()).unwrap();
    db.execute("CREATE (:X)").unwrap();
    assert!(!std::path::Path::new(":memory:").exists());
    assert_eq!(count(&mut db, "X"), 1);
}

// F14: property names starting with `__` collide with the metadata keys
// the engine binds next to user properties (`n.__id`, `r.__src`, ...) and
// corrupt results (wrong ids, reversed paths). They are reserved.
const RESERVED_MSG: &str = "SemanticError(InvalidArgumentValue) at runtime: \
     property names starting with `__` are reserved for graphdblite metadata";

#[test]
fn reserved_property_names_are_rejected_by_cypher_writes() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:A {k: 1})-[:R]->(:B)").unwrap();
    for q in [
        "CREATE (:N {__id: 99})",
        "CREATE (:A)-[:R {__src: 1}]->(:B)",
        "MATCH (a:A) SET a.__labels = 1",
        "MATCH (a:A) SET a += {__type: 'X'}",
        "MATCH (a:A) SET a = {__edge_seq: 1}",
        "MATCH ()-[r:R]->() SET r.__dst = 5",
        "MERGE (m:M {__anything: 1})",
    ] {
        let err = db.execute(q).unwrap_err();
        assert_eq!(err.to_string(), RESERVED_MSG, "query: {q}");
    }
    // Nothing was written by the rejected statements.
    let rows = db
        .execute("MATCH (n) RETURN count(n) AS c, sum(size(keys(n))) AS k")
        .unwrap();
    assert_eq!(rows[0].get("c"), Some(&Value::I64(2)));
    assert_eq!(rows[0].get("k"), Some(&Value::I64(1)));
}

#[test]
fn reserved_property_names_are_rejected_by_the_rust_api() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    let props = graphdblite::Properties::from([("__id".to_string(), Value::I64(1))]);
    let err = tx.create_node("A", props).unwrap_err();
    assert_eq!(err.to_string(), RESERVED_MSG);
}

// Boundary: a single leading underscore or an inner `__` is a normal name.
#[test]
fn near_reserved_property_names_are_allowed() {
    let mut db = Database::open_memory().unwrap();
    db.execute("CREATE (:A {_id: 1, a__b: 2, _: 3})").unwrap();
    let rows = db
        .execute("MATCH (a:A) RETURN a._id AS x, a.a__b AS y, a._ AS z")
        .unwrap();
    assert_eq!(rows[0].get("x"), Some(&Value::I64(1)));
    assert_eq!(rows[0].get("y"), Some(&Value::I64(2)));
    assert_eq!(rows[0].get("z"), Some(&Value::I64(3)));
}
