//! End-to-end lifecycle scenario for composite (multi-column) indexes.
//!
//! Earlier tests cover planner correctness (`composite_index_planner_tests.rs`)
//! and DDL grammar (`e2e_query_tests.rs::cypher_composite_index_create_and_use`).
//! This file exercises the full write-side maintenance path: create → SET → DELETE
//! → drop + recreate (backfill), all via Cypher.

use graphdblite::{Database, Value};

#[test]
fn composite_index_full_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("composite.db")).unwrap();

    // Create the index first, then nodes — exercises update_indexes_for_node.
    db.execute("CREATE INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'a', name: 'Alice'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'b', name: 'Bob'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'a', name: 'Carol'})")
        .unwrap();
    // Node with one of the indexed properties missing — must be skipped at index time.
    db.execute("CREATE (:Person {tenant_id: 9, name: 'Partial'})")
        .unwrap();

    // Full match.
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'a'}) RETURN p.name AS n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Value::String("Alice".into())));

    // Prefix match (leading column only).
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1}) RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&Value::I64(2)));

    // SET the leading column: index must replace the old entry with the new one.
    db.execute("MATCH (p:Person {tenant_id: 2, ext_id: 'a'}) SET p.tenant_id = 99")
        .unwrap();
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 99, ext_id: 'a'}) RETURN p.name AS n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Value::String("Carol".into())));
    // Old key gone — tenant_id=2 should now return zero rows.
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 2}) RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&Value::I64(0)));

    // DROP + recreate exercises backfill over the pre-existing nodes.
    db.execute("DROP INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();
    db.execute("CREATE INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();

    // db.indexes() reports 2 rows (one per covered column).
    let rows = db
        .execute("CALL db.indexes() YIELD label, property, kind RETURN label, property, kind")
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.get("label"), Some(&Value::String("Person".into())));
        assert_eq!(row.get("kind"), Some(&Value::String("btree".into())));
    }

    // Backfill survives — the three fully-propertied nodes (tenant_id=1/ext='a',
    // tenant_id=1/ext='b', tenant_id=99/ext='a') are re-indexed; Partial is skipped.
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1}) RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&Value::I64(2)));
    let rows = db
        .execute("MATCH (p:Person {tenant_id: 99, ext_id: 'a'}) RETURN p.name AS n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Value::String("Carol".into())));
}

/// WHERE-clause equality predicates (`MATCH (p:L) WHERE p.a=1 AND p.b=2`)
/// must drive composite-index selection the same way inline pattern
/// properties (`MATCH (p:L {a:1, b:2})`) do. Earlier versions handled
/// only the inline form and pushed WHERE predicates one at a time
/// through the single-prop path, so composite indexes never fired
/// against a WHERE conjunction.
#[test]
fn composite_index_picked_for_where_clause_equality() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("composite_where.db")).unwrap();

    db.execute("CREATE INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'a', name: 'Alice'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'b', name: 'Bob'})")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 2, ext_id: 'a', name: 'Carol'})")
        .unwrap();

    // Both columns in WHERE — should hit the composite at full width.
    let rows = db
        .execute("MATCH (p:Person) WHERE p.tenant_id = 1 AND p.ext_id = 'a' RETURN p.name AS n")
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Value::String("Alice".into())));

    // Leading column only in WHERE — composite as prefix scan.
    let rows = db
        .execute("MATCH (p:Person) WHERE p.tenant_id = 1 RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&Value::I64(2)));

    // Mixed: leading-column equality + non-equality on the trailing column.
    // Equality must drive the prefix lookup; the non-equality stays in the
    // residual filter.
    let rows = db
        .execute(
            "MATCH (p:Person) \
             WHERE p.tenant_id = 1 AND p.ext_id <> 'a' \
             RETURN p.name AS n",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("n"), Some(&Value::String("Bob".into())));
}

/// `exec_delete` calls `remove_indexes_for_node` + `remove_fts_for_node`
/// before removing each node, so composite (and single-prop, and FTS)
/// indexes stay consistent through the Cypher DELETE path. This used to
/// be a latent bug: `exec_delete` called `storage::node::delete_node`
/// directly, leaving stale index entries; composite indexes surfaced it
/// as `NodeNotFound` from the planner-picked prefix lookup.
#[test]
fn composite_index_cleared_on_cypher_delete() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("composite_delete.db")).unwrap();

    db.execute("CREATE INDEX ON :Person(tenant_id, ext_id)")
        .unwrap();
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 'a'})")
        .unwrap();
    db.execute("MATCH (p:Person {tenant_id: 1, ext_id: 'a'}) DELETE p")
        .unwrap();

    let rows = db
        .execute("MATCH (p:Person {tenant_id: 1, ext_id: 'a'}) RETURN count(p) AS n")
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&Value::I64(0)));
}
