//! Regression tests for write paths that mutate an indexed property or label
//! but historically skipped secondary/FTS index maintenance, producing stale
//! index entries and silent wrong query results.
//!
//! Each test drives the mutation through Cypher and then asserts via an
//! index-served query (equality on the indexed property) AND a full label scan,
//! so a stale/missing index entry surfaces as a row-count mismatch. Every test
//! fails against the pre-fix implementation.

use graphdblite::Database;

/// Count rows returned by a query, committing the read tx.
fn count(db: &mut Database, cypher: &str) -> usize {
    let tx = db.read_tx().unwrap();
    let rows = tx.query(cypher).unwrap();
    tx.commit().unwrap();
    rows.len()
}

// ── #1: MERGE ON MATCH / ON CREATE SET map/label bypassed index maintenance ──

#[test]
fn merge_on_match_set_map_merge_maintains_index() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :Person(name)").unwrap();
        tx.query("CREATE (:Person {id: 1})").unwrap();
        // ON MATCH SET n += {name: 'Bob'} must write the index entry for `name`.
        tx.query("MERGE (n:Person {id: 1}) ON MATCH SET n += {name: 'Bob'}")
            .unwrap();
        tx.commit().unwrap();
    }
    // Index-served lookup and label scan must agree: exactly one 'Bob'.
    let via_index = count(&mut db, "MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.id");
    let via_scan = count(
        &mut db,
        "MATCH (n:Person) WHERE n.name = 'Bob' OR n.id = -1 RETURN n.id",
    );
    assert_eq!(
        via_index, 1,
        "index-served lookup missed the MERGE-updated row"
    );
    assert_eq!(via_scan, 1, "label scan should also see exactly one row");
}

#[test]
fn merge_on_match_set_map_overwrite_maintains_index() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :Person(name)").unwrap();
        tx.query("CREATE (:Person {id: 1, name: 'Old'})").unwrap();
        // SET n = {..} replaces all props; index for old 'Old' must be removed
        // and 'New' inserted.
        tx.query("MERGE (n:Person {id: 1}) ON MATCH SET n = {id: 1, name: 'New'}")
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        count(&mut db, "MATCH (n:Person) WHERE n.name = 'New' RETURN n.id"),
        1,
        "new value not indexed after SET = map"
    );
    assert_eq!(
        count(&mut db, "MATCH (n:Person) WHERE n.name = 'Old' RETURN n.id"),
        0,
        "stale index entry for the overwritten value still served"
    );
}

#[test]
fn merge_on_create_set_map_maintains_index() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :Person(name)").unwrap();
        // Node does not exist -> ON CREATE fires.
        tx.query("MERGE (n:Person {id: 1}) ON CREATE SET n += {name: 'Fresh'}")
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        count(
            &mut db,
            "MATCH (n:Person) WHERE n.name = 'Fresh' RETURN n.id"
        ),
        1,
        "ON CREATE SET map did not maintain the index"
    );
}

#[test]
fn merge_on_match_set_label_reindexes_under_added_primary_label() {
    // Adding a label alphabetically-smaller than the existing one shifts the
    // primary label that owns the index entry.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        // Index on the label that will BECOME primary after the SET.
        tx.query("CREATE INDEX ON :Admin(id)").unwrap();
        tx.query("CREATE (:Person {id: 1})").unwrap();
        tx.query("MERGE (n:Person {id: 1}) ON MATCH SET n:Admin")
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        count(&mut db, "MATCH (n:Admin) WHERE n.id = 1 RETURN n.id"),
        1,
        ":Admin(id) index not populated after SET n:Admin via MERGE"
    );
}

// ── #5: SET n:Label / REMOVE n:Label skipped re-indexing ─────────────────────

#[test]
fn set_label_reindexes_when_primary_label_shifts() {
    // CREATE INDEX ON :Person(email); then SET n:Admin makes 'Admin' the new
    // primary; a later property write must land in the still-correct index.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :Person(email)").unwrap();
        tx.query("CREATE (n:Person {email: 'a@b'})").unwrap();
        // Add an alphabetically-smaller label -> primary shifts to 'Admin'.
        tx.query("MATCH (n:Person {email: 'a@b'}) SET n:Admin")
            .unwrap();
        // Now update the indexed property.
        tx.query("MATCH (n:Person) SET n.email = 'c@d'").unwrap();
        tx.commit().unwrap();
    }
    // The :Person(email) index must reflect the new value and not the old one.
    assert_eq!(
        count(
            &mut db,
            "MATCH (n:Person) WHERE n.email = 'c@d' RETURN n.email"
        ),
        1,
        "index-served lookup missed the updated email after label shift"
    );
    assert_eq!(
        count(
            &mut db,
            "MATCH (n:Person) WHERE n.email = 'a@b' RETURN n.email"
        ),
        0,
        "stale 'a@b' index entry served a phantom row after label shift"
    );
}

#[test]
fn remove_label_reindexes_when_primary_label_shifts() {
    // Node with labels [Admin, Person] (primary Admin). Index lives on Person.
    // Removing Admin makes Person primary; the Person index must then own the
    // entry and serve lookups.
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :Person(email)").unwrap();
        tx.query("CREATE (n:Admin:Person {email: 'x@y'})").unwrap();
        // With primary = Admin, the Person index has no entry yet; after
        // REMOVE n:Admin, primary becomes Person and the entry must exist.
        tx.query("MATCH (n:Person) REMOVE n:Admin").unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        count(
            &mut db,
            "MATCH (n:Person) WHERE n.email = 'x@y' RETURN n.email"
        ),
        1,
        "Person index not populated after REMOVE of the prior primary label"
    );
}

// ── #3: single-prop index table-name collision across underscore boundaries ──

/// Regression (#3): with an index on `:A_b(c)`, a node stored under a *different*
/// label `:A` with property `b_c` must NOT leak through the `:A_b(c)` index.
/// The legacy `node_idx_{label}_{prop}` scheme mapped `(A_b, c)` and `(A, b_c)`
/// to the same physical table, so an index-served `MATCH (n:A_b {c: 999})`
/// wrongly returned the `:A` node.
#[test]
fn index_lookup_does_not_leak_across_underscore_colliding_labels() {
    let mut db = Database::open_memory().unwrap();
    {
        let tx = db.write_tx().unwrap();
        tx.query("CREATE INDEX ON :A_b(c)").unwrap();
        tx.query("CREATE (:A_b {c: 111})").unwrap();
        tx.query("CREATE (:A {b_c: 999})").unwrap();
        tx.commit().unwrap();
    }
    // Index-served lookup for :A_b(c)=999 must find nothing (the 999 lives on
    // an :A node under property b_c, a different index).
    assert_eq!(
        count(&mut db, "MATCH (n:A_b) WHERE n.c = 999 RETURN n.c"),
        0,
        "cross-label node leaked through a colliding index table"
    );
    // The legitimately-indexed node is still found.
    assert_eq!(
        count(&mut db, "MATCH (n:A_b) WHERE n.c = 111 RETURN n.c"),
        1,
        "the real :A_b(c) node was not served by its index"
    );
}

/// Regression (#3, secondary symptom): creating an index whose canonical table
/// name would have collided with an existing one must succeed, not spuriously
/// report IndexAlreadyExists.
#[test]
fn create_index_on_underscore_colliding_pair_succeeds() {
    let mut db = Database::open_memory().unwrap();
    let tx = db.write_tx().unwrap();
    tx.query("CREATE INDEX ON :A_b(c)").unwrap();
    // Under the legacy scheme this collided onto node_idx_A_b_c and failed.
    tx.query("CREATE INDEX ON :A(b_c)").unwrap();
    tx.commit().unwrap();
}

// ── Systemic guard: index/FTS consistency across ALL node-mutating ops ───────
//
// The "index ⟺ storage" invariant: an index-served equality query must return
// exactly the same nodes as a full label scan with the same predicate. This
// matrix runs every mutating Cypher operation and checks that invariant, on
// both single-label and multi-label nodes (so `labels.first()` primary shifts
// are always exercised). It is the standing guard against any future write path
// that forgets index/FTS maintenance — the class behind findings #1 and #5.

/// Assert that an equality query on an indexed property returns the index-served
/// count AND matches both the label scan and the caller's expected count.
///
/// Checking against `expected` (not just index-vs-scan agreement) is essential:
/// a maintenance bug can leave BOTH the index empty and make the scan miss the
/// row for the same reason, so index==scan alone can silently pass. The absolute
/// expected count anchors the invariant to storage truth.
fn assert_indexed_count(
    db: &mut Database,
    label: &str,
    prop: &str,
    value_literal: &str,
    expected: usize,
) {
    // Index-served: bare equality (planner picks IndexLookup when an index
    // exists on `label(prop)`).
    let indexed = count(
        db,
        &format!("MATCH (n:{label}) WHERE n.{prop} = {value_literal} RETURN n.{prop}"),
    );
    // Force a scan by OR-ing a never-true predicate so the whole WHERE is not a
    // single equality the planner can push into an IndexLookup.
    let scanned = count(
        db,
        &format!(
            "MATCH (n:{label}) WHERE n.{prop} = {value_literal} OR n.__never = 1 RETURN n.{prop}"
        ),
    );
    assert_eq!(
        scanned, expected,
        "label scan for :{label}({prop}) = {value_literal} expected {expected}, got {scanned}"
    );
    assert_eq!(
        indexed, scanned,
        "index/scan disagree for :{label}({prop}) = {value_literal}: index={indexed}, scan={scanned}"
    );
}

#[test]
fn index_stays_consistent_across_all_mutations_single_label() {
    run_index_matrix(&["Person"]);
}

#[test]
fn index_stays_consistent_across_all_mutations_multi_label() {
    // Two labels; 'AAA' sorts before 'Person' so the primary label is 'AAA',
    // while the index lives on 'Person' — the multi-label case that broke #5.
    run_index_matrix(&["AAA", "Person"]);
}

fn run_index_matrix(labels: &[&str]) {
    let colon_labels = labels.join(":");

    // Each case: create a fresh DB + index, apply one mutation, then assert the
    // exact index-served count for the new value (must be 1) and the old value
    // (must be 0 — no stale/phantom entry). `setup` runs the write; the trailing
    // (value, expected) pairs are checked.
    let run = |setup: &str, checks: &[(&str, usize)]| {
        let mut db = Database::open_memory().unwrap();
        {
            let tx = db.write_tx().unwrap();
            tx.query("CREATE INDEX ON :Person(v)").unwrap();
            for stmt in setup.split('\n').filter(|s| !s.trim().is_empty()) {
                tx.query(&stmt.replace("{LABELS}", &colon_labels)).unwrap();
            }
            tx.commit().unwrap();
        }
        for (val, expected) in checks {
            assert_indexed_count(&mut db, "Person", "v", val, *expected);
        }
    };

    // SET n.p = v — replaces the indexed value.
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMATCH (n:Person) WHERE n.id = 1 SET n.v = 20",
        &[("20", 1), ("10", 0)],
    );
    // SET n += {..} (plain, write.rs path) — sets a previously-absent value.
    run(
        "CREATE (n:{LABELS} {id: 1})\nMATCH (n:Person) WHERE n.id = 1 SET n += {v: 20}",
        &[("20", 1)],
    );
    // SET n = {..} — full overwrite; old indexed value must be dropped.
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMATCH (n:Person) WHERE n.id = 1 SET n = {id: 1, v: 20}",
        &[("20", 1), ("10", 0)],
    );
    // REMOVE n.p — indexed value must disappear.
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMATCH (n:Person) WHERE n.id = 1 REMOVE n.v",
        &[("10", 0)],
    );
    // MERGE ON MATCH SET n.p = v (merge.rs Property arm).
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMERGE (n:Person {id: 1}) ON MATCH SET n.v = 20",
        &[("20", 1), ("10", 0)],
    );
    // MERGE ON MATCH SET n += {..} (merge.rs MapMerge arm — finding #1).
    run(
        "CREATE (n:{LABELS} {id: 1})\nMERGE (n:Person {id: 1}) ON MATCH SET n += {v: 20}",
        &[("20", 1)],
    );
    // MERGE ON MATCH SET n = {..} (merge.rs MapOverwrite arm — finding #1).
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMERGE (n:Person {id: 1}) ON MATCH SET n = {id: 1, v: 20}",
        &[("20", 1), ("10", 0)],
    );
    // MERGE ON CREATE SET n += {..} (merge.rs MapMerge arm, create branch).
    run(
        "MERGE (n:Person {id: 1}) ON CREATE SET n += {v: 20}",
        &[("20", 1)],
    );
    // DELETE — no phantom entry left behind.
    run(
        "CREATE (n:{LABELS} {id: 1, v: 10})\nMATCH (n:Person) WHERE n.id = 1 DELETE n",
        &[("10", 0)],
    );
}
