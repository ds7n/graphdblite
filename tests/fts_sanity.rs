//! tests/fts_sanity.rs
//!
//! Confirms the bundled SQLite has FTS5 with the trigram tokenizer
//! compiled in. If this test fails the rest of the fulltext plan is moot.

use rusqlite::Connection;

#[test]
fn fts5_trigram_is_available() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE t USING fts5(content, tokenize='trigram case_sensitive 1');\n\
         INSERT INTO t(rowid, content) VALUES (1, 'foobar');",
    )
    .expect("FTS5 + trigram tokenizer must be available in bundled SQLite");

    let mut stmt = conn
        .prepare("SELECT rowid FROM t WHERE content MATCH ?")
        .unwrap();
    let ids: Vec<i64> = stmt
        .query_map(["\"foo\""], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(ids, vec![1]);
}
