/// Integration tests for the composite-index planner (Task 8).
///
/// Tests that require `create_composite_index` on `Database` / `WriteTransaction`
/// are marked `#[ignore]` until Task 9 lands that public API.  The tie-break
/// test uses only single-prop indexes and exercises the planner's
/// `pick_index_for_equality_preds` selection logic today.
use graphdblite::{Database, Value};

// ---------------------------------------------------------------------------
// Tests that require Task 9 (Database::create_composite_index)
// ---------------------------------------------------------------------------

/// After Task 9: composite index covers both equality predicates — planner
/// should emit an IndexLookup over the full 2-column prefix, returning exactly
/// the one matching node.
#[test]
fn composite_index_picked_for_full_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("composite.db");
    let mut db = Database::open(&path).unwrap();

    db.begin_write().unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'alice', name: 'Alice'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'bob', name: 'Bob'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'alice', name: 'Other'})")
        .unwrap();
    db.commit().unwrap();

    {
        let tx = db.write_tx().unwrap();
        tx.create_composite_index("Person", &["tenant_id", "ext_id"])
            .unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'alice'}) RETURN p.name")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("p.name").unwrap(),
        &Value::String("Alice".into())
    );
}

/// After Task 9: composite index exists on (tenant_id, ext_id); query only
/// provides the leading column — planner picks a prefix scan, returning 2 rows.
#[test]
fn composite_index_picked_for_prefix_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("composite_prefix.db");
    let mut db = Database::open(&path).unwrap();

    db.begin_write().unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'alice'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'bob'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'alice'})")
        .unwrap();
    db.commit().unwrap();

    {
        let tx = db.write_tx().unwrap();
        tx.create_composite_index("Person", &["tenant_id", "ext_id"])
            .unwrap();
        tx.commit().unwrap();
    }

    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1}) RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n").unwrap(), &Value::I64(2));
}

// ---------------------------------------------------------------------------
// Tests that run today (single-prop indexes only)
// ---------------------------------------------------------------------------

/// When two single-prop indexes exist and the WHERE clause covers both, the
/// planner should pick one of them (whichever `pick_index_for_equality_preds`
/// selects) and return correct results via an IndexLookup.
///
/// This verifies no regression to the prior "most-selective cardinality"
/// single-prop path: with a single matching node the query must still return
/// exactly one row.
#[test]
fn single_prop_index_still_works_after_refactor() {
    let mut db = Database::open_memory().unwrap();

    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "name").unwrap();
        tx.commit().unwrap();
    }

    db.begin_write().unwrap();
    db.execute("CREATE (:Person {name: 'Alice', age: 30})")
        .unwrap();
    db.execute("CREATE (:Person {name: 'Bob', age: 25})")
        .unwrap();
    db.commit().unwrap();

    let rows = db
        .execute("MATCH (p:Person {name: 'Alice'}) RETURN p.age")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("p.age").unwrap(), &Value::I64(30));
}

/// With two single-prop indexes (tenant_id and ext_id) and a WHERE clause
/// covering both, the planner must still pick exactly one index and produce
/// correct filtered results.  The remaining non-indexed predicate becomes a
/// residual filter on the IndexLookup output.
///
/// This is the "longer-prefix wins" test that *can* run today: the two-column
/// case without a composite index falls back to single-prop selection, so we
/// just verify correctness, not which index was chosen.
#[test]
fn two_single_prop_indexes_both_predicates_returns_correct_rows() {
    let mut db = Database::open_memory().unwrap();

    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "tenant_id").unwrap();
        tx.create_index("Person", "ext_id").unwrap();
        tx.commit().unwrap();
    }

    db.begin_write().unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'alice', n: 1})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'bob', n: 2})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'alice', n: 3})")
        .unwrap();
    db.commit().unwrap();

    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'alice'}) RETURN p.n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("p.n").unwrap(), &Value::I64(1));
}

/// Regression (#6): contradictory equality predicates on the same indexed
/// property (`n.x = 1 AND n.x = 2`) are unsatisfiable and must return zero rows.
/// The index-pushdown path previously folded only the last value into the
/// IndexLookup and dropped the other conjunct from the residual filter, so an
/// indexed query wrongly returned the `x = 2` node. The result must match the
/// no-index Filter path (0 rows) and be independent of conjunct order.
#[test]
fn contradictory_equality_on_indexed_prop_returns_zero_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contradiction.db");
    let mut db = Database::open(&path).unwrap();

    db.begin_write().unwrap();
    db.execute("CREATE (:Person {x: 1}), (:Person {x: 2})")
        .unwrap();
    db.commit().unwrap();

    {
        let tx = db.write_tx().unwrap();
        tx.create_index("Person", "x").unwrap();
        tx.commit().unwrap();
    }

    // Index-served: must be unsatisfiable.
    let rows = db
        .execute("MATCH (n:Person) WHERE n.x = 1 AND n.x = 2 RETURN n.x")
        .unwrap();
    assert_eq!(
        rows.len(),
        0,
        "contradictory equality returned rows via index"
    );

    // Order-independent: the reversed conjunction must also be empty.
    let rows_rev = db
        .execute("MATCH (n:Person) WHERE n.x = 2 AND n.x = 1 RETURN n.x")
        .unwrap();
    assert_eq!(rows_rev.len(), 0, "result depends on conjunct order");

    // Sanity: a satisfiable single-value lookup still works via the index.
    let ok = db
        .execute("MATCH (n:Person) WHERE n.x = 2 RETURN n.x")
        .unwrap();
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].get("n.x").unwrap(), &Value::I64(2));
}
