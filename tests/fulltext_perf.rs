//! Perf gate for fulltext-accelerated CONTAINS.
//!
//! Generates 10k Doc nodes with deterministic random strings, then runs a
//! CONTAINS query that matches a small subset. Asserts completion within a
//! generous time bound. Fails loudly if the planner stops rewriting
//! CONTAINS to FullTextLookup, or if the FTS lookup itself regresses.

use graphdblite::Database;
use std::time::Instant;

fn random_string(seed: u64) -> String {
    let mut s = String::with_capacity(20);
    let mut x = seed;
    for _ in 0..20 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let c = (b'a' + ((x >> 33) % 26) as u8) as char;
        s.push(c);
    }
    s
}

#[test]
fn contains_with_fulltext_index_is_fast() {
    let mut db = Database::open_memory().unwrap();
    db.begin_write().unwrap();
    db.create_fulltext_index("Doc", "body").unwrap();
    for i in 0..10_000u64 {
        let s = random_string(i);
        db.execute(&format!("CREATE (:Doc {{body: '{s}'}})"))
            .unwrap();
    }
    // Marker with a distinctive substring.
    db.execute("CREATE (:Doc {body: 'aaa marker xyz'})")
        .unwrap();
    db.commit().unwrap();

    let t0 = Instant::now();
    let rows = db
        .execute("MATCH (n:Doc) WHERE n.body CONTAINS 'marker' RETURN n.body AS b")
        .unwrap();
    let elapsed = t0.elapsed();
    assert_eq!(rows.len(), 1);
    assert!(
        elapsed.as_millis() < 500,
        "CONTAINS with fulltext index took {elapsed:?}; regression?"
    );
}
