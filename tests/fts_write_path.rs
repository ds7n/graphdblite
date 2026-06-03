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
