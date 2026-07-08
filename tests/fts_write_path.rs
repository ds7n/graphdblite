//! End-to-end check that FTS indexes stay in sync with node writes
//! through the public Database API.

use graphdblite::Database;

#[test]
fn fts_index_reflects_node_lifecycle() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (n:Doc {body: 'alpha bravo charlie'})")
        .unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();

    // Via the planner rewrite + executor: CONTAINS hits FullTextLookup.
    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'bravo' RETURN n.body AS b")
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Insert a new node — verify the write-path hook fires AND the index serves it.
    db.begin_write().unwrap();
    db.execute("CREATE (n:Doc {body: 'delta echo foxtrot'})")
        .unwrap();
    db.commit().unwrap();
    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'echo' RETURN n.body AS b")
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Drop the index — DDL succeeds; queries still work via scan.
    db.begin_write().unwrap();
    db.drop_fulltext_index("Doc", "body").unwrap();
    db.commit().unwrap();
    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'bravo' RETURN n.body AS b")
        .unwrap();
    assert_eq!(rows.len(), 1);
}

/// Regression: single-prop FTS table names must not collide across underscore
/// boundaries. `(A_b, c)` and `(A, b_c)` previously both mapped to
/// `node_fts_A_b_c`, so a `CONTAINS` query on `:A_b(c)` could match an `:A`
/// node's `b_c`, and creating the second index spuriously reported
/// IndexAlreadyExists.
#[test]
fn fts_single_prop_table_names_do_not_collide_across_underscores() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.execute("CREATE (:A_b {c: 'needle text here'})").unwrap();
    db.execute("CREATE (:A {b_c: 'needle text here'})").unwrap();
    db.create_fulltext_index("A_b", "c").unwrap();
    // Under the legacy scheme this collided onto node_fts_A_b_c and errored.
    db.create_fulltext_index("A", "b_c").unwrap();
    db.commit().unwrap();

    // CONTAINS on :A_b(c) must match only the :A_b node, never the :A node.
    let rows = db
        .execute("MATCH (n:A_b) WHERE n.c CONTAINS 'needle' RETURN n.c AS c")
        .unwrap();
    assert_eq!(rows.len(), 1, "FTS lookup leaked across colliding labels");
}
